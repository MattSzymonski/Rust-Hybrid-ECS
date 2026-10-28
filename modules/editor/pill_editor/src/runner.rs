//! Native editor executable: the egui frontend's frame loop, window and
//! device.
//!
//! # Responsibilities
//!
//! - Own the `winit` event loop, the main window, the scene views' child
//!   windows, and the editor's own GPU device that draws the UI.
//! - Host the project and its extensions - including the reloadable egui
//!   shell module - through `pill_host`.
//! - Drive one world frame per redraw: push the window events into the
//!   engine input, publish [`EguiShellInput`], run the host's frame (the
//!   shell's egui pass runs inside it), then draw the shell's
//!   [`EguiShellFrame`] through the composition overlay.
//! - Publish the shared selection as the renderer's outline set and the
//!   transform gizmo's handles for it, and turn scene clicks into picks whose
//!   answers move the selection or start a gizmo drag.
//!
//! # Design
//!
//! The window redraws on request - every presented frame asks for the next,
//! so the engine steps while the window is visible and the compositor idles a
//! hidden one. The shell boundary is plain data in the world (the shared
//! `pill_editor_core::boundary` resources), so the frontend and the reloadable
//! shell module never share a type that could dangle across a reload, and with
//! no shell module loaded the runner draws a minimal egui fallback of its own.

// Standard library
use std::sync::Arc;
use std::time::Duration;

// External crates
use pill_core::platform::futures::block_on;
use pill_core::platform::Instant;
use pill_core::telemetry::telemetry_target;
use pill_core::{info, warn};
use pill_editor_core::assets::{self, AssetDialog};
use pill_editor_core::boundary::{
    frame_from_output, EguiPopoutInputs, EguiShellFrame, EguiShellInput, ShellBadge, ShellRect,
    DEFAULT_SCENE_VIEWS,
};
use pill_editor_core::cadence::{Cadence, FrameDemand, FrameWake};
use pill_editor_core::camera::{self, CameraButton, CameraNavigation, Modifiers, OrbitCamera};
use pill_editor_core::clipboard::{self, ClipboardAction, ClipboardRequest, EditorClipboard};
use pill_editor_core::commands::{
    ensure_command_resources, read_gizmo_settings, write_gizmo_preferences, CommandRegistry,
    EditorPreferences, KeybindingsFile, KeymapPreset, PendingEditorCommands, KEYBINDINGS_FILE,
    PREFERENCES_FILE,
};
use pill_editor_core::console::{ConsoleChannel, ConsoleLine, ConsoleState};
use pill_editor_core::drain::{self, HistoryDirection};
use pill_editor_core::editor_state::{
    AssetDetail, AssetListing, AssetReferenceField, EditorCommand, EditorSelection, EditorSnapshot,
    EntityPickRequest, SelectionClick, StandaloneTypeInfo,
};
use pill_editor_core::egui;
use pill_editor_core::entities;
use pill_editor_core::gizmo::{
    self, GizmoGeometry, GizmoMode, GizmoSettings, GizmoSpace, GizmoState, HandleKind, HandleStates,
};
use pill_editor_core::history::CommandHistory;
use pill_editor_core::inspector;
use pill_editor_core::layout::{
    layout_directory, EditorTab, BOTTOM_ROW_FRACTION, SIDE_COLUMN_FRACTION,
};
use pill_editor_core::notices::{EditorNotices, Notice, NoticeLevel, ProblemRow};
use pill_editor_core::panels::{
    ModuleLiveness, PanelCommand, PanelCommands, PanelRegistrations, PanelRegistry, PanelRequest,
};
use pill_editor_core::preferences::{self, PreferencesView};
use pill_editor_core::previews;
use pill_editor_core::problems::{self as problem_parser, Severity};
use pill_editor_core::scripted::{
    AssetEdit, Dump, KeyChord, PaletteRequest, ScriptedCommand, ToolbarAction,
};
use pill_editor_core::session::{EguiShellState, HistoryRequest};
use pill_editor_core::style::{
    layers_text, merge_layers, ThemeLayer, ThemeLayerInfo, UserThemeSelection,
};
use pill_editor_core::systems;
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
use pill_ui_theme::{load_overrides, ThemeOverrides, ThemePathScope};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::raw_window_handle::HasWindowHandle;
use winit::window::{Window, WindowId};

// Current crate
use crate::frontend::EditorError;
use crate::overlay::OverlayMode;
use crate::overlay::OverlaySurface;
use crate::test_channel::{self, TestChannel};

// =============================================================================
// Constants
// =============================================================================

/// The environment variable that forces the window-surface posture.
pub const NO_OVERLAY_ENV: &str = "PILL_EDITOR_NO_OVERLAY";

/// Whether [`NO_OVERLAY_ENV`] asks for the window-surface posture.
///
/// The switch, like the continuous-redraw one, is on for anything but an empty
/// string or `0`, so `PILL_EDITOR_NO_OVERLAY=1` in a shell is enough.
fn no_overlay_requested() -> bool {
    std::env::var(NO_OVERLAY_ENV)
        .map(|value| !value.is_empty() && value != "0")
        .unwrap_or(false)
}

/// Build the UI's surface in the posture the editor starts in.
///
/// The composition overlay is the normal one and the window surface is the
/// fallback: a refusal is logged once and the editor keeps running, with the
/// UI on the window's own surface.
fn create_overlay(
    no_overlay: bool,
    instance: &wgpu::Instance,
    adapter: &wgpu::Adapter,
    device: &wgpu::Device,
    window: Arc<Window>,
    size: (u32, u32),
    scale_factor: f32,
) -> Option<OverlaySurface> {
    if no_overlay {
        warn!(
            target: telemetry_target::ENGINE,
            variable = NO_OVERLAY_ENV,
            "[editor] no overlay posture: the UI is drawn on the window surface"
        );
        return create_window_overlay(instance, adapter, device, window, size);
    }
    match OverlaySurface::new(
        instance,
        adapter,
        device,
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
                "[editor] composition overlay unavailable; drawing the UI on the window surface"
            );
            create_window_overlay(instance, adapter, device, window, size)
        }
    }
}

/// Build the window-surface overlay, logging the refusal that stops it.
fn create_window_overlay(
    instance: &wgpu::Instance,
    adapter: &wgpu::Adapter,
    device: &wgpu::Device,
    window: Arc<Window>,
    size: (u32, u32),
) -> Option<OverlaySurface> {
    match OverlaySurface::new_window(instance, adapter, device, window, EDITOR_FORMAT, size) {
        Ok(overlay) => Some(overlay),
        Err(error) => {
            warn!(
                target: telemetry_target::ENGINE,
                error = %error,
                "[editor] window surface overlay unavailable; the UI will not be drawn"
            );
            None
        }
    }
}

/// The shell module's crate name - the editor-only module's convention.
const SHELL_MODULE_NAME: &str = "pill_editor_shell_egui_editor";

/// The window's initial size in logical pixels.
const INITIAL_WINDOW_SIZE: LogicalSize<f64> = LogicalSize::new(1280.0, 800.0);

/// The window's minimum size in logical pixels: the dock's minimum layout
/// plus room for the menu bar, so a drag of the window's edge cannot leave the
/// panels unusable.
const MINIMUM_WINDOW_SIZE: LogicalSize<f64> = LogicalSize::new(960.0, 600.0);

/// The scene view's initial size in logical pixels.
const INITIAL_SCENE_SIZE: LogicalSize<f64> = LogicalSize::new(640.0, 480.0);

/// A popped-out tab window's initial size in logical pixels: tall and narrow
/// enough for the Inspector and the Assets panel, and the person resizes it
/// from there.
const POPOUT_INITIAL_SIZE: LogicalSize<f64> = LogicalSize::new(520.0, 720.0);

/// The scene views' surface ids: view `n` draws through surface `n + 1`, so
/// each view keeps its surface however many views the person opens. Surface
/// 0 is the renderer's primary, which the game window carries.
fn scene_surface(view: usize) -> SurfaceId {
    SurfaceId(1 + view as u32)
}

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

/// The hover test's default radius around a handle, as a fraction of the
/// handle length: roughly the width the overlay draws a line with. The
/// preferences' `editor.gizmo_hit_radius` overrides it, and both the hover
/// and the press read the one value.
const GIZMO_HIT_RADIUS_DEFAULT: f32 = preferences::GIZMO_HIT_RADIUS_DEFAULT;

/// One dash (and one gap) of the translation displacement line, as a
/// fraction of the handle length.
const GIZMO_DASH: f32 = 0.05;

/// Frames between periodic asset-listing walks; a file command refreshes
/// the listing at once instead of waiting for the cadence.
const ASSET_REFRESH_FRAMES: u64 = 120;

/// Frames between recovery-document writes when nothing it watches changed:
/// a slow cadence that still lands an arrangement the key cannot see, without
/// rewriting the file every frame.
const RECOVERY_AUTOSAVE_FRAMES: u64 = 120;

/// Frames between unconditional re-reads of the editor's state files:
/// a frame reads `preferences.json` and `keybindings.json` only when their
/// metadata moved, so this is the longest an edit can take to reach the editor
/// on a filesystem whose timestamps are too coarse to see it.
const STATE_FILE_POLL_FRAMES: u64 = 30;

/// How many walked asset rows the listing keeps, before the module group
/// rows the frame adds on top.
const ASSET_ROWS_MAX: usize = 256;

/// The editor's format: the overlay's swapchain and egui's renderer must
/// share it, and composition swapchains offer it.
const EDITOR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8UnormSrgb;

// =============================================================================
// Run
// =============================================================================

/// Run the editor until the last window closes.
///
/// `log_buffer` is the console's capture ring: every frame the editor folds
/// the lines it has not seen into the Console tab's history.
pub fn run(log_buffer: pill_core::telemetry::capture::LogBuffer) -> Result<(), EditorError> {
    let event_loop =
        EventLoop::new().map_err(|source| EditorError::EventLoopCreation { source })?;
    let mut application = EditorApplication {
        window: None,
        log_buffer,
        timer: WakeTimer::new(),
    };
    event_loop
        .run_app(&mut application)
        .map_err(|source| EditorError::EventLoopRun { source })
}

/// The `winit` application: one editor window, created on resume.
struct EditorApplication {
    /// The window and its host, once the loop resumed.
    window: Option<EditorWindow>,
    /// The telemetry capture ring the window's Console tab reads.
    log_buffer: pill_core::telemetry::capture::LogBuffer,
    /// The loop's timed wake, when the last frame asked for one.
    timer: WakeTimer,
}

/// The loop's timed wake: the moment the next frame is owed.
///
/// `std::time::Instant` is the type `ControlFlow::WaitUntil` takes; this
/// frontend is native-only, so the platform clock's web-safety rule does not
/// reach here (the deadline itself is never compared across targets).
#[allow(clippy::disallowed_types)]
struct WakeTimer {
    /// When the loop must wake, when it is waiting for a deadline.
    deadline: Option<std::time::Instant>,
}

#[allow(clippy::disallowed_types)]
impl WakeTimer {
    /// A timer with nothing scheduled.
    fn new() -> Self {
        Self { deadline: None }
    }

    /// Schedule the next wake `after` from now.
    fn schedule(&mut self, after: Duration) {
        self.deadline = Some(std::time::Instant::now() + after);
    }

    /// Forget any scheduled wake: something asked for a frame now.
    fn clear(&mut self) {
        self.deadline = None;
    }

    /// Whether the deadline has passed, clearing it when it has.
    fn take_due(&mut self) -> bool {
        let Some(deadline) = self.deadline else {
            return false;
        };
        if std::time::Instant::now() >= deadline {
            self.deadline = None;
            return true;
        }
        false
    }

    /// What the loop should do until the next event.
    fn control_flow(&self) -> ControlFlow {
        match self.deadline {
            Some(deadline) => ControlFlow::WaitUntil(deadline),
            None => ControlFlow::Wait,
        }
    }
}

impl ApplicationHandler for EditorApplication {
    /// Create the editor window on the first resume.
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        match EditorWindow::open(event_loop, self.log_buffer.clone()) {
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
        // A popped-out tab's window: its events are its own
        // pass's, never the main window's - a click in it lands at that
        // window's coordinates.
        let popout_tab = editor
            .popouts
            .iter()
            .find(|(_tab, popout)| popout.window.id() == window_id)
            .map(|(tab, _popout)| tab.clone());
        if !is_main && child_view.is_none() && !is_game && popout_tab.is_none() {
            return;
        }
        // A popped-out window only needs its own resize and close handling
        // here; its redraws ride the main window's frame.
        if let Some(tab) = &popout_tab {
            match &event {
                WindowEvent::Resized(size) => {
                    if let Some(popout) = editor.popouts.get_mut(tab) {
                        popout
                            .surface
                            .resize(&editor.device, (size.width, size.height));
                    }
                }
                WindowEvent::CloseRequested => {
                    editor.close_popout(tab);
                }
                _ => {}
            }
        }
        let redraw_request = matches!(event, WindowEvent::RedrawRequested);
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
                match editor.frame(event_loop) {
                    Ok(should_exit) => {
                        if should_exit {
                            event_loop.exit();
                        } else {
                            match editor.next_wake() {
                                FrameWake::Immediate => {
                                    self.timer.clear();
                                    editor.window.request_redraw();
                                }
                                FrameWake::After(delay) => {
                                    // The loop sleeps instead of drawing:
                                    // input still wakes it, and the deadline
                                    // covers the host's own changes.
                                    self.timer.schedule(delay);
                                }
                            }
                            event_loop.set_control_flow(self.timer.control_flow());
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
                editor.demand.resized = true;
                if let Some(view) = child_view {
                    editor.resize_scene(view, size.width, size.height);
                } else if is_game {
                    editor.resize_game(size.width, size.height);
                } else {
                    editor.resize(size.width, size.height);
                }
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                editor.demand.resized = true;
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
                if let WindowEvent::KeyboardInput { event, .. } = &other {
                    editor.gizmo_shortcut(event);
                }
                // A file dragged in from the OS: every
                // editor window reports it - the scene views must not swallow
                // the drop - and the executable is what turns it into an
                // import. These events carry no pointer or key state, so they
                // are handled here and the rest of the walk is skipped.
                let mut handled = false;
                match &other {
                    WindowEvent::HoveredFile(path) => {
                        editor.preview_drop(path);
                        handled = true;
                    }
                    WindowEvent::HoveredFileCancelled => {
                        editor.cancel_drop_preview();
                        handled = true;
                    }
                    WindowEvent::DroppedFile(path) => {
                        editor.drop_files(vec![path.clone()]);
                        handled = true;
                    }
                    _ => {}
                }
                if handled {
                    if editor.demand.any() {
                        self.timer.clear();
                        editor.window.request_redraw();
                        event_loop.set_control_flow(ControlFlow::Wait);
                    }
                    return;
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
                // The scene and game windows' pointer positions are relative
                // to their own client areas, while the engine - which also
                // feeds the shell's egui pass - works in the main window's
                // client coordinates. The window's placed origin bridges the
                // two, so hovering and clicking the overlay strip works.
                let origin = match child_view {
                    Some(view) => editor.view_origin(view),
                    None if is_game => editor.game_origin(),
                    None => (0.0, 0.0),
                };
                // The focus rule: the game's window is the
                // game's, so its events become the engine's input and the
                // shell never sees them. The editor's own windows - the main
                // one and the scene views - are the UI's: their events feed
                // the shell, and a project system never receives a keystroke
                // meant for the Inspector. A popped-out tab's window is its
                // own pass's: its events go to that tab's
                // record and no further.
                for event in translate_window_event(&other) {
                    let event = offset_pointer(event, origin);
                    if let Some(tab) = &popout_tab {
                        editor.popout_event(tab, event);
                        continue;
                    }
                    match route_event(&event, is_game) {
                        EventRoute::Game => editor.pending_events.push(event),
                        EventRoute::Shell => {
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
                // Input is why a waiting loop wakes: the
                // event itself asks for the frame that consumes it.
                editor.demand.input = true;
            }
        }
        // A frame is owed as soon as anything here happened; a redraw request
        // already is one.
        if editor.demand.any() && !redraw_request {
            self.timer.clear();
            editor.window.request_redraw();
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }

    /// The loop is about to sleep: wake the editor when its deadline passed.
    ///
    /// The timed wake is what keeps an idle editor's Console tab and notices
    /// current without drawing frames the display never sees.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.timer.take_due() {
            if let Some(editor) = &mut self.window {
                editor.window.request_redraw();
            }
        }
        event_loop.set_control_flow(self.timer.control_flow());
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
    /// renderer keeps its own device inside the host.
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
    /// The scene views' child windows, one per scene tab, in view-index
    /// order: each has its own camera and renderer surface. A slot is `None`
    /// when its window could not be created, and the vector grows when the
    /// dock reports a view it does not hold yet.
    views: Vec<Option<SceneView>>,
    /// The game viewport's window, when one could be created: it carries
    /// the primary surface, so the project's renderer presents the game
    /// view there. `None` leaves the primary on the main window.
    game_surface: Option<SurfaceWindow>,
    /// One window per popped-out tab, keyed by the tab's
    /// wire name: each carries its own swapchain and its own egui renderer,
    /// because the UI that draws into it is a pass of its own.
    popouts: std::collections::BTreeMap<String, PopoutWindow>,
    /// The editor's cameras, one per scene viewport: navigation moves the
    /// active viewport's camera only, and every scene ray projects through
    /// its own view's camera.
    cameras: Vec<OrbitCamera>,
    /// The viewport the last scene click activated; camera navigation and
    /// the gizmo read it.
    active_view: usize,
    /// Whether each scene view's left button is down: a pick answers a frame
    /// or two after the press, and a handle grab only starts a drag while
    /// the button is still held.
    left_down: Vec<bool>,
    /// The camera navigation drag in flight, when one is.
    camera_drag: Option<CameraDrag>,
    /// The keyboard modifiers held, as last reported by a modifiers event.
    modifiers: Modifiers,
    /// The keymap preset the preferences name: the navigation table in force.
    keymap: KeymapPreset,
    /// Whether each view's published camera binding is stale: navigation
    /// marks it and the next frame re-publishes the surface camera.
    camera_dirty: Vec<bool>,
    /// egui's own renderer on the editor's device.
    egui_renderer: egui_wgpu::Renderer,
    /// The composition overlay the UI is presented through; `None` until it
    /// is created, and left `None` when the platform refuses it.
    overlay: Option<OverlaySurface>,
    /// `PILL_EDITOR_NO_OVERLAY` asked for the window-surface posture, so the
    /// composition overlay is never retried.
    no_overlay: bool,
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
    scene_cursor: Vec<Option<(f32, f32)>>,
    /// The scene left press waiting for its pick answer: the view, the press
    /// position and the pick id. The answer starts a gizmo drag on a handle
    /// or moves the selection - both while the button is held.
    press_pick: Option<PressPick>,
    /// The entities the renderer's outline set currently names, so the world
    /// resource is written once per selection change.
    outlined_entities: Vec<Entity>,
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
    /// The handle hit radius as a fraction of the handle length, from the
    /// preferences file: the hover test and the press share it.
    gizmo_hit_radius: f32,
    /// Whether the preferences file asks for rendered asset previews, and the
    /// paths whose preview is on its way to the renderer.
    render_previews: bool,
    preview_in_flight: std::collections::HashMap<u64, String>,
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
    /// The last walk's own rows, before the module roots' synthetic folders
    /// are added: what the next walk is compared against to tell whether the
    /// tree moved.
    asset_walk: Vec<AssetListing>,
    /// The live standalone asset types the create dialog offers.
    standalone_types: Vec<StandaloneTypeInfo>,
    /// Whether a file command changed the tree: the next frame's listing
    /// refresh walks it at once.
    asset_listing_dirty: bool,
    /// The file a drag from the OS is carrying over the editor, if one is: the
    /// drop preview's subject.
    drop_preview: Option<std::path::PathBuf>,
    /// The recovery document a previous run left behind,
    /// applied on the first frame the shell's session exists.
    recovered: Option<pill_editor_core::layout::RecoveryDocument>,
    /// The state the last recovery write captured, so an unchanged editor
    /// does not rewrite the document every frame.
    last_recovery_key: Option<String>,
    /// Whether the main window currently takes IME input,
    /// and the caret rectangle it was last given.
    ime_allowed: bool,
    /// The IME candidate window's anchor, in logical points.
    ime_rect: Option<(f32, f32, f32, f32)>,
    /// The selected asset's detail for the Inspector's asset section.
    asset_detail: Option<AssetDetail>,
    /// The path `asset_detail` was captured for, so the capture follows the
    /// selection without a disk read every frame.
    asset_detail_path: Option<String>,
    /// The preferences file's text last read; the live reader applies the
    /// history limits and the snapping steps when it changes.
    preferences_text: Option<String>,
    /// The preferences file's metadata when it was last read, so a frame only
    /// reads a file that moved.
    preferences_stamp: FileStamp,
    /// The keybindings file's text last read; a change re-applies the preset
    /// and the overrides to the command registry.
    keybindings_text: Option<String>,
    /// The keybindings file's metadata when it was last read.
    keybindings_stamp: FileStamp,
    /// Whether the keybindings watcher ran once: the first pass applies the
    /// editor's default preset even when there is no file to differ from it.
    keybindings_applied: bool,
    /// The telemetry capture ring the Console tab reads.
    log_buffer: pill_core::telemetry::capture::LogBuffer,
    /// The parsed preferences document the snapshot's view and the scripted
    /// dump build on.
    preferences_document: serde_json::Map<String, serde_json::Value>,
    /// The theme the editor's UI paints with: the code
    /// default merged with the user's file and the project's layer.
    theme: pill_ui_theme::Theme,
    /// How many themes the layers have resolved to; the shell installs a new
    /// one when this moves and leaves the current one alone when it does not.
    theme_generation: u64,
    /// The layers [`Self::theme`] was merged from, in apply order.
    theme_layers: Vec<ThemeLayerInfo>,
    /// The font bytes the layers resolved: the snapshot
    /// carries them beside the theme, so the shell installs both together.
    theme_fonts: pill_editor_core::kit::style::ThemeFonts,
    /// What the last theme resolution read, so a poll only merges when a
    /// layer moved.
    theme_watch: ThemeWatch,
    /// The generation the fallback context paints, so the no-shell posture
    /// follows a theme change the same way the shell does.
    fallback_theme_generation: u64,
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
    /// The cursor egui asked for in the last frame, so a change is logged
    /// once instead of every frame: the resize cursors over a splitter are how
    /// the widened grab band shows itself.
    last_cursor: egui::CursorIcon,
    /// The scripted command channel, when the environment names one.
    test_channel: Option<TestChannel>,
    /// A scripted left-button drag in flight, when one is.
    scripted_drag: Option<ScriptedDrag>,
    /// The scene title last reported, so the log names each state the
    /// document went through once instead of every frame.
    scene_title: String,
    /// Quit after this many seconds, when the environment asks for it.
    test_seconds: Option<f64>,
    /// A quit the scripted channel or the deadline asked for.
    quit: bool,
    /// When the loop draws next.
    cadence: Cadence,
    /// What has asked for the next frame since the last one ran.
    demand: FrameDemand,
    /// The decision the last frame left for the loop.
    next_wake: FrameWake,
    /// egui's own repaint request from the last shell pass.
    repaint_after: Option<Duration>,
    /// What asked for the last frame, kept for the cadence dump.
    last_demand: FrameDemand,
    /// The entity badges this frame's scene views draw:
    /// every visible view's entities, projected to logical points.
    badges: Vec<ShellBadge>,
    /// Whether the last decision left the loop in the polled state, so the
    /// `[editor] frame idle` line appears once per transition.
    polling: bool,
    /// The most expensive frame drawn while polling since the last
    /// heartbeat, in milliseconds: the measured cost of an idle frame.
    idle_frame_millis_max: f32,
    /// The snapshot field whose change last asked for a frame, so the log
    /// names a churning reason once rather than every frame.
    snapshot_changed_field: Option<String>,
    /// One liveness marker per module that registered a panel. The executable
    /// owns the strong handle, so a module reload - which the registry is told
    /// about - is what makes its panels read "module not loaded"; the module
    /// itself only queues requests.
    panel_markers: std::collections::HashMap<String, Arc<ModuleLiveness>>,
    /// The newest request per panel id, kept so a panel can be registered
    /// again after an unload without the module running.
    panel_requests: Vec<PanelRequest>,
}

/// What the last theme resolution read.
///
/// The executable is the side that resolves the layers: it reads the
/// `theme` preference, the user's file, `editor_settings.yaml` and the
/// project's layer through the asset store. Keeping what each read produced
/// is what lets a poll skip the merge when nothing moved.
#[derive(Default)]
struct ThemeWatch {
    /// The preference's value, as last resolved.
    selection: UserThemeSelection,
    /// The user file's metadata when it was last read.
    user_stamp: FileStamp,
    /// The legacy name already reported, so one value logs once.
    legacy_logged: Option<String>,
    /// `editor_settings.yaml`'s metadata when it was last read.
    settings_stamp: FileStamp,
    /// The settings file's last read, or the message that refused it.
    settings: Option<Result<pill_host::EditorSettings, String>>,
    /// The project layer's path, as last resolved.
    project_path: Option<String>,
    /// The project layer's file metadata when it was last read.
    project_stamp: FileStamp,
    /// The project layer's handle in the asset store.
    project_handle: Option<pill_engine_core::Handle<ThemeOverrides>>,
    /// The handle's content version when it was last merged.
    project_version: Option<u64>,
    /// Where each family's font comes from, as last resolved (body, display,
    /// monospace).
    font_sources: [Option<FontSource>; 3],
    /// The resolved source files' metadata when they were last read.
    font_stamps: [FileStamp; 3],
    /// The font bytes the sources gave, as last installed.
    fonts: pill_editor_core::kit::style::ThemeFonts,
}

/// Where one font family's bytes come from.
#[derive(Clone, Debug, PartialEq, Eq)]
enum FontSource {
    /// A `.ttf`/`.otf` below `res`, loaded through the project's asset store -
    /// the project layer's fonts.
    Project(String),
    /// A file beside the user's theme document, read from disk.
    User(std::path::PathBuf),
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
    fn open(
        event_loop: &ActiveEventLoop,
        log_buffer: pill_core::telemetry::capture::LogBuffer,
    ) -> Result<EditorWindow, EditorError> {
        // Step 1: the main window. A minimum size keeps an accidental drag of
        // the window's own edge from squeezing the dock unusable.
        let attributes = Window::default_attributes()
            .with_title("Pill")
            .with_inner_size(INITIAL_WINDOW_SIZE)
            .with_min_inner_size(MINIMUM_WINDOW_SIZE);
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
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::DX12,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            compatible_surface: None,
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))
        .map_err(|source| EditorError::AdapterRequest { source })?;
        let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("pill editor (egui)"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            memory_hints: wgpu::MemoryHints::default(),
            trace: wgpu::Trace::Off,
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
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
        let egui_renderer = egui_wgpu::Renderer::new(
            &device,
            EDITOR_FORMAT,
            egui_wgpu::RendererOptions::default(),
        );
        let no_overlay = no_overlay_requested();
        let overlay = create_overlay(
            no_overlay,
            &instance,
            &adapter,
            &device,
            Arc::clone(&window),
            size,
            scale_factor,
        );
        if let Some(overlay) = &overlay {
            info!(
                target: telemetry_target::ENGINE,
                window_surface = matches!(overlay.mode(), OverlayMode::Window),
                "[editor] overlay posture"
            );
        }
        // Step 4: the game viewport's window first - it carries the primary
        // surface, so the project renders through its own camera into a
        // child window the Game tab shows through. A refusal leaves the
        // primary on the main window and the Game tab merely mirrors what is
        // already drawn there.
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
        // The window names the project, as the project's own settings file
        // spells it; the open scene is the document's state and is reported
        // through the scene-title log instead.
        window.set_title(&format!("Pill - {}", config.name));
        let mut host =
            pill_host::setup_rendering(config, primary_window, primary_size.0, primary_size.1)?;
        info!(
            target: telemetry_target::ENGINE,
            detached = game_surface.as_ref().is_some_and(|game| game.detached),
            width = primary_size.0,
            height = primary_size.1,
            "[editor] game surface attached"
        );
        // The gizmo tool's starting state: the project's own
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
        // The frontend owns the drags; the tool's mode, space and snapping
        // change through the shortcuts, the scripted channel and the recorded
        // `SetGizmoTool` edits, and the file's settings stand until one does.
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
        // The editor's project layer loads through the project's asset store,
        // so the `.theme` type is registered here even when the project itself
        // installs no theme.
        pill_ui_theme::register(world);
        // The Console tab's history and filters: filled each frame from the
        // telemetry capture ring.
        world.register_resource::<ConsoleState>();
        if world.get_resource::<ConsoleState>().is_none() {
            world.insert_resource(ConsoleState::default());
        }
        // The module panels: the registry, the queue modules
        // register through and the commands a panel asked for. All three exist
        // before any module loads, so a module's `register` has somewhere to
        // put its request and the shell has something to draw from.
        world.register_resource::<PanelRegistry>();
        if world.get_resource::<PanelRegistry>().is_none() {
            world.insert_resource(PanelRegistry::default());
        }
        world.register_resource::<PanelRegistrations>();
        if world.get_resource::<PanelRegistrations>().is_none() {
            world.insert_resource(PanelRegistrations::default());
        }
        world.register_resource::<PanelCommands>();
        if world.get_resource::<PanelCommands>().is_none() {
            world.insert_resource(PanelCommands::default());
        }
        // The shell's side of the OS clipboard: the shell
        // queues a request, this executable's Win32 bridge carries it out.
        world.register_resource::<EditorClipboard>();
        if world.get_resource::<EditorClipboard>().is_none() {
            world.insert_resource(EditorClipboard::default());
        }
        // Step 5: the scene views' child windows and their renderer
        // surfaces, each with its own camera (navigation moves the active
        // viewport's). Both start hidden; each shows when the dock shows its
        // tab.
        let cameras = vec![OrbitCamera::default(); DEFAULT_SCENE_VIEWS];
        let mut views: Vec<Option<SceneView>> = (0..DEFAULT_SCENE_VIEWS).map(|_| None).collect();
        for view in 0..DEFAULT_SCENE_VIEWS {
            let Some(mut scene) = SceneView::open(event_loop, &window, view) else {
                continue;
            };
            let scene_size = scene.window.size();
            scene.attach(&mut host, &cameras[view], scene_size);
            views[view] = Some(scene);
        }
        // The scene grid: the editor's view-bound viewports draw
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
            popouts: std::collections::BTreeMap::new(),
            cameras,
            active_view: 0,
            left_down: vec![false; DEFAULT_SCENE_VIEWS],
            camera_drag: None,
            modifiers: Modifiers::default(),
            keymap: KeymapPreset::default(),
            camera_dirty: vec![true; DEFAULT_SCENE_VIEWS],
            egui_renderer,
            overlay,
            no_overlay,
            overlay_retry_frame: 0,
            fallback_context: {
                // The fallback draws the same theme as the shell, so a
                // missing shell module does not flip the look.
                let context = egui::Context::default();
                pill_editor_core::style::apply(&context);
                context
            },
            reported_fallback: false,
            pending_events: Vec::new(),
            shell_events: Vec::new(),
            shell_cursor: None,
            notices: EditorNotices::default(),
            scene_cursor: vec![None; DEFAULT_SCENE_VIEWS],
            press_pick: None,
            outlined_entities: Vec::new(),
            overlay_regions: Vec::new(),
            gizmo_drag: None,
            gizmo_geometry: GizmoGeometry::default(),
            hovered_handle: None,
            gizmo_reference: 0.0,
            gizmo_hit_radius: GIZMO_HIT_RADIUS_DEFAULT,
            render_previews: false,
            preview_in_flight: std::collections::HashMap::new(),
            badges: Vec::new(),
            gizmo_guides: None,
            gizmo_logged: None,
            synthetic_press_ids: Vec::new(),
            synthetic_gizmo_pending: None,
            synthetic_gizmo_hold: false,
            synthetic_gizmo_press: None,
            synthetic_gizmo_view: None,
            asset_entries: Vec::new(),
            asset_walk: Vec::new(),
            standalone_types: Vec::new(),
            asset_listing_dirty: true,
            drop_preview: None,
            recovered: pill_editor_core::layout::read_recovery(),
            last_recovery_key: None,
            ime_allowed: false,
            ime_rect: None,
            asset_detail: None,
            asset_detail_path: None,
            preferences_text: None,
            preferences_stamp: FileStamp::default(),
            keybindings_text: None,
            keybindings_stamp: FileStamp::default(),
            keybindings_applied: false,
            log_buffer,
            preferences_document: serde_json::Map::new(),
            // The code default until the first poll resolves the layers; the
            // generation matches what `EguiShellState::new` installed, so a
            // project with no layers never re-applies at startup.
            theme: pill_editor_core::style::default_theme(),
            theme_generation: 1,
            theme_layers: vec![ThemeLayerInfo::code()],
            theme_fonts: pill_editor_core::kit::style::ThemeFonts::default(),
            theme_watch: ThemeWatch::default(),
            fallback_theme_generation: 1,
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
            last_cursor: egui::CursorIcon::Default,
            test_channel: TestChannel::from_environment(),
            scripted_drag: None,
            scene_title: "untitled".to_string(),
            test_seconds: test_channel::seconds_from_environment(),
            quit: false,
            cadence: {
                let cadence = Cadence::from_environment();
                if cadence.continuous() {
                    info!(
                        target: telemetry_target::ENGINE,
                        variable = pill_editor_core::cadence::CONTINUOUS_REDRAW_ENV,
                        "[editor] redraw continuous"
                    );
                } else {
                    info!(
                        target: telemetry_target::ENGINE,
                        idle_poll_ms = pill_editor_core::cadence::IDLE_POLL.as_millis() as u64,
                        "[editor] redraw on demand"
                    );
                }
                cadence
            },
            demand: FrameDemand::default(),
            next_wake: FrameWake::Immediate,
            repaint_after: None,
            last_demand: FrameDemand::default(),
            polling: false,
            idle_frame_millis_max: 0.0,
            snapshot_changed_field: None,
            panel_markers: std::collections::HashMap::new(),
            panel_requests: Vec::new(),
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
    /// its window - a stale render target draws the scene at the wrong size
    /// inside the tab. The placement cache is cleared, so the next layout pass
    /// re-applies the dock's rectangle.
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

    /// Open the scene views the dock reports that this editor does not hold
    /// yet.
    ///
    /// The shell reports one slot per view index the dock carries, so a view
    /// the person just added is the list's last slot. Each new view gets its
    /// own window, surface and camera here - the same three the startup views
    /// get - and starts hidden: the placement that follows this frame shows
    /// it.
    fn ensure_scene_views(&mut self, count: usize, event_loop: &ActiveEventLoop) {
        let wanted = count.min(pill_editor_core::boundary::SCENE_VIEWS_MAX);
        while self.views.len() < wanted {
            let view = self.views.len();
            self.cameras.push(OrbitCamera::default());
            self.left_down.push(false);
            self.camera_dirty.push(true);
            self.scene_cursor.push(None);
            let opened = SceneView::open(event_loop, &self.window, view);
            if let Some(mut scene) = opened {
                let size = scene.window.size();
                scene.attach(&mut self.host, &self.cameras[view], size);
                info!(
                    target: telemetry_target::ENGINE,
                    view,
                    surface = scene.surface_id.0,
                    "[editor] scene view opened"
                );
                self.views.push(Some(scene));
            } else {
                // A view without a window keeps its slots: the dock still
                // holds its tab, and the tab shows the UI's chips alone.
                self.views.push(None);
            }
        }
    }

    /// Place the first scene view at the dock's default layout's center
    /// inside the client area, until the shell's first scene rect arrives;
    /// the other views stay hidden until their tabs show.
    fn place_views(&mut self) {
        let Some(Some(scene)) = self.views.first_mut() else {
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

    /// Give a popped-out scene view its top-level window, placed at the
    /// pop-out's screen position. The first call replaces its
    /// child window with a top-level one - a child can never reach another
    /// monitor - and later calls just follow the pop-out as it moves.
    fn pop_view_window(
        &mut self,
        event_loop: &ActiveEventLoop,
        view: usize,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) {
        let needs_window = self
            .views
            .get(view)
            .and_then(|scene| scene.as_ref())
            .is_some_and(|scene| !scene.popped);
        if needs_window {
            let title = format!("Pill Editor Scene {} (popped out)", view + 1);
            match SurfaceWindow::open_top_level(event_loop, &title) {
                Ok(window) => {
                    let host = &mut self.host;
                    if let Some(Some(scene)) = self.views.get_mut(view) {
                        scene.rebind_window(host, window, true);
                    }
                }
                Err(error) => {
                    warn!(
                        target: telemetry_target::ENGINE,
                        view,
                        error = %error,
                        "[editor] popped-out scene window refused"
                    );
                    return;
                }
            }
        }
        if let Some(Some(scene)) = self.views.get_mut(view) {
            scene.window.place_screen(x, y, width, height);
        }
    }

    /// Give a scene view its child window back when its tab returns to the
    /// dock. A view the platform forced to a detached window is left alone:
    /// that window is its dock window, not a pop-out's.
    fn ensure_view_docked(&mut self, event_loop: &ActiveEventLoop, view: usize) {
        let popped = self
            .views
            .get(view)
            .and_then(|scene| scene.as_ref())
            .is_some_and(|scene| scene.popped);
        if !popped {
            return;
        }
        let title = format!("Pill Editor Scene {}", view + 1);
        match SurfaceWindow::open(event_loop, &self.window, &title, "scene") {
            Ok(window) => {
                let host = &mut self.host;
                if let Some(Some(scene)) = self.views.get_mut(view) {
                    scene.rebind_window(host, window, false);
                }
            }
            Err(error) => warn!(
                target: telemetry_target::ENGINE,
                view,
                error = %error,
                "[editor] scene view redock refused"
            ),
        }
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

    /// Ask what is under a left press, while the button is held: a handle the
    /// hover test names starts its drag in this same press, and only a miss
    /// queues the object pick.
    ///
    /// The CPU test answers first: the pick's answer arrives a frame
    /// later and only covers a couple of pixels, so a handle that highlighted
    /// under the pointer could lose the press to the pick and clear the
    /// selection. One hitbox, read by the hover and the press alike, is what
    /// keeps the highlight and the grab in agreement.
    fn press_scene(&mut self, view: usize, x: f32, y: f32) {
        if self.gizmo_drag.is_none() {
            if let Some(handle) = self.handle_at(view, x, y) {
                info!(
                    target: telemetry_target::ENGINE,
                    view,
                    handle,
                    path = "handle",
                    "[editor] gizmo press"
                );
                self.set_hovered_handle(view, Some(handle));
                self.begin_gizmo_drag(view, handle, x, y);
                return;
            }
        }
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
            path = "pick",
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

    /// The hovered handle from the pointer's ray, against the handles the
    /// last frame drew. The radius follows the handle length, so it reads
    /// like the drawn line rather than a fixed world distance, and the
    /// preference scales it.
    fn update_hover(&mut self, view: usize, point: (f32, f32)) {
        if self.gizmo_drag.is_some() || self.camera_drag.is_some() {
            return;
        }
        let hit = self.handle_at(view, point.0, point.1);
        self.set_hovered_handle(view, hit);
    }

    /// The handle under one pixel of one view, by the CPU ray test.
    ///
    /// The hover highlight and the press both answer through this one call,
    /// so a handle that lights up under the pointer is a handle the press
    /// grabs.
    fn handle_at(&self, view: usize, x: f32, y: f32) -> Option<u32> {
        if self.gizmo_geometry.lines.is_empty() && self.gizmo_geometry.triangles.is_empty() {
            return None;
        }
        let (ray_origin, ray_direction) = self.scene_ray(view, x, y)?;
        let radius = (self.gizmo_reference * self.gizmo_hit_radius).max(1e-4);
        gizmo::hit_test(&self.gizmo_geometry, ray_origin, ray_direction, radius)
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
        Some(requests.request(scene_surface(view), x, y))
    }

    /// Apply the answers to scene picks: an entity moves the shared selection
    /// onto it, empty space clears the selection, and a handle starts a gizmo
    /// drag. The outline follows from the selection, and the handles from it,
    /// on the next publish.
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
                    // An armed Inspector row takes the click as its reference
                    // and leaves the selection alone: the person is naming a
                    // target, not changing what they are editing.
                    let armed = world
                        .get_resource_mut::<EntityPickRequest>()
                        .and_then(|request| request.take());
                    if let Some(target) = armed {
                        let (edit, label) = inspector::entity_edit(
                            target.entity,
                            &target.component,
                            &target.field,
                            Some(entity),
                        );
                        let coalesce = inspector::field_coalesce_key(
                            target.entity,
                            &target.component,
                            &target.field,
                        );
                        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
                            pending.push_recorded(vec![edit], label, Some(coalesce));
                        }
                        info!(
                            target: telemetry_target::ENGINE,
                            component = target.component.as_str(),
                            field = target.field.as_str(),
                            "[editor] inspector entity pick answered"
                        );
                    } else if let Some(selection) = world.get_resource_mut::<EditorSelection>() {
                        if selection.primary() != Some(entity) || selection.entities().len() != 1 {
                            selection.select_only(entity);
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
                        selection.clear_entities();
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

    /// Move the previews the renderer finished into the shell's textures, then
    /// ask for the next tile's.
    ///
    /// One request at a time: the renderer draws one preview per frame and
    /// reads it back between frames, so a grid of tiles fills in over frames
    /// rather than stalling one. A tile only asks once - its image is kept in
    /// the session until the listing changes - and the queue is the grid's own
    /// list of what it drew, so scrolling costs work only for what is visible.
    fn apply_asset_previews(&mut self) {
        let images = {
            let world = self.host.host_mut().engine_mut().world_mut();
            world
                .get_resource_mut::<pill_runtime::PreviewResults>()
                .map(|results| results.take())
                .unwrap_or_default()
        };
        if !images.is_empty() {
            let world = self.host.host_mut().engine_mut().world_mut();
            let Some(state) = world.get_resource_mut::<EguiShellState>() else {
                return;
            };
            for image in images {
                let Some(path) = self.preview_in_flight.remove(&image.id) else {
                    continue;
                };
                // A refusal carries no pixels: the tile keeps its type icon
                // and the grid moves on, rather than waiting on this file
                // forever.
                if image.is_empty() {
                    state.preview_refused.insert(path.clone());
                    info!(
                        target: telemetry_target::ENGINE,
                        path = path.as_str(),
                        id = image.id,
                        "[editor] preview refused"
                    );
                    continue;
                }
                let size = [image.width as usize, image.height as usize];
                let texture = state.context.load_texture(
                    format!("preview:{path}"),
                    egui::ColorImage::from_rgba_unmultiplied(size, &image.rgba),
                    egui::TextureOptions::LINEAR,
                );
                state.previews.insert(path.clone(), texture);
                info!(
                    target: telemetry_target::ENGINE,
                    path = path.as_str(),
                    width = image.width,
                    height = image.height,
                    "[editor] preview ready"
                );
            }
        }
        if !self.render_previews || !self.preview_in_flight.is_empty() {
            return;
        }
        // Every visible tile still waiting for an image, in draw order.
        let waiting: Vec<String> = {
            let world = self.host.host_mut().engine_mut().world_mut();
            let Some(state) = world.get_resource::<EguiShellState>() else {
                return;
            };
            state
                .preview_visible
                .iter()
                .filter(|key| {
                    !state.previews.contains_key(*key) && !state.preview_refused.contains(*key)
                })
                .cloned()
                .collect()
        };
        if waiting.is_empty() {
            return;
        }
        // A texture preview names its file, so the request needs the asset
        // root the listing's paths are relative to.
        let root = self
            .host
            .host_mut()
            .asset_directory()
            .map(std::path::Path::to_path_buf);
        let Some(root) = root else {
            return;
        };
        // The first tile the listing can name a preview for. A row with none -
        // a folder, a scene, a file type the renderer cannot draw - is
        // recorded as such, because a tile that is never answered would block
        // every tile drawn after it: the grid lists folders first, so one
        // folder used to starve a whole folder of files.
        let mut skipped = Vec::new();
        let mut next = None;
        for key in waiting {
            match previews::preview_asset_for(self.asset_entries.iter(), &key, &root) {
                Some(asset) => {
                    next = Some((key, asset));
                    break;
                }
                None => skipped.push(key),
            }
        }
        if !skipped.is_empty() {
            let world = self.host.host_mut().engine_mut().world_mut();
            if let Some(state) = world.get_resource_mut::<EguiShellState>() {
                for key in &skipped {
                    state.preview_refused.insert(key.clone());
                }
            }
            for key in &skipped {
                info!(
                    target: telemetry_target::ENGINE,
                    path = key.as_str(),
                    "[editor] preview skipped"
                );
            }
        }
        let Some((path, asset)) = next else {
            return;
        };
        let id = {
            let world = self.host.host_mut().engine_mut().world_mut();
            pill_runtime::ensure_preview_resources(world);
            let Some(requests) = world.get_resource_mut::<pill_runtime::PreviewRequests>() else {
                return;
            };
            requests.request(asset)
        };
        self.preview_in_flight.insert(id, path.clone());
        info!(
            target: telemetry_target::ENGINE,
            path = path.as_str(),
            id,
            "[editor] preview requested"
        );
    }

    /// Project every visible entity into every visible scene view, for the
    /// shell's badges.
    ///
    /// The executable owns the views' cameras and the renderer's projection
    /// terms, so it is the only side that can say where an entity lands on
    /// screen. Each view is capped at
    /// [`pill_editor_core::boundary::BADGES_PER_VIEW_MAX`] and an entity
    /// behind the camera or outside its viewport is left out, so the overlay
    /// stays a map of the scene rather than a wall of chips.
    fn publish_badges(&mut self) {
        self.badges.clear();
        let rows = {
            let world = self.host.host_mut().engine_mut().world_mut();
            world.entity_rows()
        };
        for view in 0..self.views.len() {
            if self.views[view]
                .as_ref()
                .is_none_or(|scene| !scene.window.visible)
            {
                continue;
            }
            let Some((width, height)) = self.scene_pixel_size(view) else {
                continue;
            };
            let (origin_x, origin_y) = self.view_origin(view);
            let mut drawn = 0usize;
            for row in &rows {
                if drawn >= pill_editor_core::boundary::BADGES_PER_VIEW_MAX {
                    break;
                }
                let Some(translation) = self.entity_translation(row) else {
                    continue;
                };
                let Some((x, y)) = self.project_to_screen_in(view, translation) else {
                    continue;
                };
                if x < 0.0 || y < 0.0 || x > width || y > height {
                    continue;
                }
                self.badges.push(ShellBadge {
                    view,
                    x: (origin_x + x) / self.scale_factor,
                    y: (origin_y + y) / self.scale_factor,
                    entity: row.entity,
                });
                drawn += 1;
            }
        }
    }

    /// One entity's world translation, read through the generic field path so
    /// the executable needs no query of its own outside a frame.
    fn entity_translation(&mut self, row: &pill_engine_core::EntityRow) -> Option<[f32; 3]> {
        if !row
            .components
            .iter()
            .any(|name| name.ends_with("TransformComponent"))
        {
            return None;
        }
        let world = self.host.host_mut().engine_mut().world_mut();
        let component = world
            .resolve_component_id_by_name_any(TRANSFORM_SHARED_NAME)
            .ok()
            .flatten()?;
        let name = world.component_registry().get_name(&component)?.to_string();
        match world.read_component_field(row.entity, &name, "translation") {
            Ok(FieldValue::Array(values)) => {
                let mut translation = [0.0f32; 3];
                for (axis, value) in values.iter().enumerate().take(3) {
                    translation[axis] = match value {
                        FieldValue::F32(number) => *number,
                        _ => return None,
                    };
                }
                Some(translation)
            }
            _ => None,
        }
    }

    /// Show what a drag from the OS is carrying: a toast
    /// naming the file and the folder it would land in, once per path, so the
    /// person sees where a drop goes before letting go.
    fn preview_drop(&mut self, path: &std::path::Path) {
        if self.drop_preview.as_deref() == Some(path) {
            return;
        }
        self.drop_preview = Some(path.to_path_buf());
        let folder = self.drop_folder();
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());
        info!(
            target: telemetry_target::ENGINE,
            path = %path.display(),
            folder = folder.as_str(),
            "[editor] drop preview"
        );
        self.push_toast(NoticeLevel::Note, format!("drop {name} into {folder}"));
    }

    /// A drag that left every window: the preview goes away.
    fn cancel_drop_preview(&mut self) {
        if self.drop_preview.take().is_some() {
            info!(
                target: telemetry_target::ENGINE,
                "[editor] drop preview cancelled"
            );
        }
    }

    /// The Assets folder a drop lands in, as the grid shows it: the session's
    /// open folder, or the asset root itself.
    fn drop_folder(&mut self) -> String {
        let world = self.host.host_mut().engine_mut().world_mut();
        let folder = world
            .get_resource::<pill_editor_core::session::EguiShellState>()
            .map(|state| state.browser.folder.clone())
            .unwrap_or_default();
        if folder.is_empty() {
            "res".to_string()
        } else {
            format!("res/{folder}")
        }
    }

    /// Import the paths a drop delivered.
    ///
    /// A folder is refused by name - the import copies files, and a directory
    /// would need a recursive walk with its own refusals - and the refusal is
    /// a toast as well as a log line. Files become one recorded
    /// `ImportFiles` step, so undo takes them off the disk again.
    fn drop_files(&mut self, paths: Vec<std::path::PathBuf>) {
        self.drop_preview = None;
        let Some(asset_root) = self
            .host
            .host_mut()
            .asset_directory()
            .map(|directory| directory.to_path_buf())
        else {
            warn!(
                target: telemetry_target::ENGINE,
                "[editor] drop refused: the host knows no asset directory"
            );
            self.push_toast(
                NoticeLevel::Warning,
                "drop refused: no asset directory".to_string(),
            );
            return;
        };
        let folder = self.drop_folder();
        let sub_folder = match pill_editor_core::browser::drop_folder_relative(&folder) {
            Ok(relative) => relative,
            Err(message) => {
                warn!(
                    target: telemetry_target::ENGINE,
                    folder = folder.as_str(),
                    "[editor] drop refused: {message}"
                );
                self.push_toast(NoticeLevel::Warning, format!("drop refused: {message}"));
                return;
            }
        };
        let destination = if sub_folder.as_os_str().is_empty() {
            asset_root.clone()
        } else {
            asset_root.join(&sub_folder)
        };
        let mut sources = Vec::new();
        for path in paths {
            if path.is_dir() {
                info!(
                    target: telemetry_target::ENGINE,
                    path = %path.display(),
                    "[editor] drop refused: folder"
                );
                self.push_toast(
                    NoticeLevel::Warning,
                    format!(
                        "{} is a folder; drop its files instead",
                        path.file_name()
                            .map(|name| name.to_string_lossy().to_string())
                            .unwrap_or_else(|| path.display().to_string())
                    ),
                );
                continue;
            }
            sources.push(path.display().to_string());
        }
        if sources.is_empty() {
            return;
        }
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push_recorded(
                vec![EditorCommand::ImportFiles {
                    sources: sources.clone(),
                    destination: destination.display().to_string(),
                }],
                "import files",
                None,
            );
        }
        self.asset_listing_dirty = true;
        info!(
            target: telemetry_target::ENGINE,
            count = sources.len(),
            folder = folder.as_str(),
            "[editor] files dropped"
        );
        self.push_toast(
            NoticeLevel::Note,
            format!("importing {} file(s) into {folder}", sources.len()),
        );
    }

    /// Apply the recovery document a previous run left behind.
    ///
    /// It is applied on the first frame the shell's session exists, because
    /// the dock lives there: the shell builds its own default-or-saved dock
    /// when it loads, and the recovered arrangement replaces it before that
    /// frame draws. The selection follows as display handles resolved against
    /// the live table - a recovered run spawns the same entities, and one that
    /// does not simply selects less.
    fn apply_recovered_state(&mut self) {
        let Some(recovered) = self.recovered.take() else {
            return;
        };
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(session) = world.get_resource_mut::<EguiShellState>() else {
            // The shell has not claimed its session yet: try again next frame.
            self.recovered = Some(recovered);
            return;
        };
        let surfaces = recovered.dock.state().surfaces_count();
        session.dock = recovered.dock.clone();
        let selection = world
            .get_resource::<EditorSelection>()
            .cloned()
            .unwrap_or_default();
        let entities: Vec<Entity> = recovered
            .entities
            .iter()
            .filter_map(|handle| resolve_display_entity(world, handle))
            .collect();
        let restored = entities.len();
        let mut selection = selection;
        selection.set_entities(entities);
        selection.asset = recovered.asset.clone();
        selection.resource = recovered.resource.clone();
        if let Some(resource) = world.get_resource_mut::<EditorSelection>() {
            *resource = selection;
        }
        info!(
            target: telemetry_target::ENGINE,
            surfaces,
            entities = restored,
            "[editor] recovered editor state after an unclean exit"
        );
    }

    /// Write the recovery document when the arrangement or the selection
    /// changed, and on a slow cadence so a change the key cannot see still
    /// lands.
    fn autosave_recovery(&mut self) {
        if !(self.frame_index.is_multiple_of(RECOVERY_AUTOSAVE_FRAMES)
            || self.last_recovery_key.is_none())
        {
            return;
        }
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(session) = world.get_resource::<EguiShellState>() else {
            return;
        };
        let selection = world
            .get_resource::<EditorSelection>()
            .cloned()
            .unwrap_or_default();
        let key = format!(
            "{}|{}|{}|{}",
            session.dock.state().surfaces_count(),
            session.dock.panel_ids().len(),
            selection
                .entities()
                .iter()
                .map(|entity| entity.to_string())
                .collect::<Vec<String>>()
                .join(","),
            selection.asset.as_deref().unwrap_or("")
        );
        if self.last_recovery_key.as_deref() == Some(key.as_str()) {
            return;
        }
        let document =
            pill_editor_core::layout::RecoveryDocument::capture(&session.dock, &selection);
        self.last_recovery_key = Some(key);
        pill_editor_core::layout::write_recovery(&document);
    }

    /// Publish the shared selection as the renderer's outline set: the orange
    /// outline drawn around every selected entity, whether the Entities tab or
    /// a scene pick moved the selection. One resource write per change, before
    /// the frame that draws it.
    fn publish_selection_outline(&mut self) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let selected: Vec<Entity> = world
            .get_resource::<EditorSelection>()
            .map(|selection| selection.entities().to_vec())
            .unwrap_or_default();
        if selected == self.outlined_entities {
            return;
        }
        self.outlined_entities = selected.clone();
        if world.get_resource::<SelectionOutlines>().is_none() {
            world.insert_resource(SelectionOutlines::default());
        }
        if let Some(outlines) = world.get_resource_mut::<SelectionOutlines>() {
            outlines.entities = selected.clone();
        }
        match selected.last() {
            Some(entity) => info!(
                target: telemetry_target::ENGINE,
                entity = %entity,
                count = selected.len(),
                "[editor] outline set"
            ),
            None => info!(
                target: telemetry_target::ENGINE,
                "[editor] outline cleared"
            ),
        }
    }

    /// Draw the gizmo handles for the selection, with their hover and drag
    /// states, the translation planes, the dynamic scale handles and the drag
    /// guides.
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
                .and_then(|selection| selection.primary());
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

    /// The tool shortcuts: a press that the active keymap binds to a tool
    /// command selects that tool.
    ///
    /// The registry owns which key means what, so a preset or a keybindings
    /// override rebinds the editor without a restart; an unbound key falls
    /// through to the rest of the editor and the game.
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
        let held = EngineModifiers {
            shift: self.modifiers.shift,
            control: self.modifiers.control,
            alt: self.modifiers.alt,
            super_key: false,
        };
        self.run_bound_command(key, held);
    }

    /// Switch the gizmo's mode; the shortcuts and the scripted `gizmo` verb
    /// land here.
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

    /// Runs the command the active keymap binds to a chord, if any, and
    /// reports whether one ran.
    ///
    /// The registry is the one source of chords: a preset or a keybindings
    /// override changes what a key does, without a restart and without a
    /// second table in the executable. A chord bound to a command this
    /// executable does not implement resolves and logs
    /// `[editor] shortcut has no action yet`, then does nothing.
    fn run_bound_command(&mut self, key: KeyCode, modifiers: EngineModifiers) -> bool {
        let world = self.host.host_mut().engine_mut().world_mut();
        CommandRegistry::ensure(world);
        let resolved = world
            .get_resource::<CommandRegistry>()
            .and_then(|registry| registry.resolve(key, modifiers))
            .map(|spec| spec.id.clone());
        let Some(command) = resolved else {
            return false;
        };
        if let Some(action) = command_action(&command) {
            info!(
                target: telemetry_target::ENGINE,
                command = command.as_str(),
                "[editor] shortcut ran"
            );
            match action {
                CommandAction::GizmoTool(mode) => self.set_gizmo_tool(mode),
                CommandAction::Undo => self.scripted_undo(),
                CommandAction::Redo => self.scripted_redo(),
                CommandAction::PauseToggle => self.scripted_pause_toggle(),
                CommandAction::Step => self.scripted_step(),
            }
        } else {
            info!(
                target: telemetry_target::ENGINE,
                command = command.as_str(),
                "[editor] shortcut has no action yet"
            );
        }
        true
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
                .and_then(|selection| selection.primary());
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
    /// straight into the world. A drag records no undo entry.
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
        self.left_down.fill(false);
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

    /// Move the shell's own pointer, as a real move arrives.
    ///
    /// egui learns a pointer position only from an event, so a suite that
    /// wants a tooltip or a resize cursor moves the pointer here. The carried
    /// cursor keeps the position on later frames, exactly like a real move
    /// that stopped moving.
    fn scripted_hover_ui(&mut self, x: f32, y: f32) {
        let position = pill_core::math::Vector2f::new(x, y);
        self.shell_cursor = Some(position);
        self.shell_events.push(InputEvent::CursorMoved { position });
        self.demand.input = true;
        info!(
            target: telemetry_target::ENGINE,
            x,
            y,
            "[editor] pointer hovered"
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
                .and_then(|selection| selection.primary());
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

    /// Log the scene grid's state: the switch, the spacing and
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

    /// Log the store's own assets - the ones no file backs, which every picker
    /// offers beside the listing's rows: the suite's window into what a picker
    /// can reach beyond the project's `res`.
    fn dump_engine_assets(&mut self) {
        let rows = {
            let world = self.host.host_mut().engine_mut().world_mut();
            world
                .get_resource::<EditorSnapshot>()
                .map(|snapshot| snapshot.engine_assets.clone())
                .unwrap_or_default()
        };
        info!(
            target: telemetry_target::ENGINE,
            count = rows.len(),
            "[test] engine assets"
        );
        for row in &rows {
            info!(
                target: telemetry_target::ENGINE,
                kind = row.kind.as_str(),
                name = row.name.as_str(),
                guid = row.guid.as_deref().unwrap_or(""),
                "[test] engine asset"
            );
        }
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
            // The root and the flag go last: the suites' patterns read the
            // other fields first, in this order.
            info!(
                target: telemetry_target::ENGINE,
                path = row.key().as_str(),
                kind = row.kind.as_str(),
                standalone = row.standalone,
                has_meta = row.has_meta,
                root = row.root.as_str(),
                read_only = row.read_only,
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
        let entries = pill_editor_core::browser::children(&self.asset_entries, &folder);
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

    /// Log the theme the editor paints with, the layers it was merged from and
    /// the generation that produced it (`dump theme`).
    ///
    /// This is the executable's own resolution - what the shell installs - so
    /// a suite can assert a layer change without reading the tab's pixels.
    fn dump_theme(&mut self) {
        let user = self
            .theme_layers
            .iter()
            .find(|info| info.layer == ThemeLayer::User)
            .map_or("none", ThemeLayerInfo::path_text);
        let project = self
            .theme_layers
            .iter()
            .find(|info| info.layer == ThemeLayer::Project)
            .map_or("none", ThemeLayerInfo::path_text);
        info!(
            target: telemetry_target::ENGINE,
            layers = %layers_text(&self.theme_layers),
            user = %user,
            project = %project,
            generation = self.theme_generation,
            accent = %self.theme.palette.accent.hex(),
            panel = %self.theme.palette.panel.hex(),
            corner = self.theme.shape.corner_radius,
            "[test] theme"
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
        // A plain click is the reset shape of the same rule, so the two scripted
        // verbs and the pointer path share one path.
        self.scripted_select_extend(handle, SelectionClick::Reset);
    }

    /// The scripted selection extension (`select add` / `select range`): the
    /// set a Ctrl- or Shift-click on the Entities list produces, recorded as
    /// the same single `SetSelection` step the row click queues.
    ///
    /// The click rule itself lives in the core's `EditorSelection::clicked`, so
    /// the pointer path and this channel cannot disagree about what a click
    /// means; what the modifiers do is the shell's `click_from_modifiers`.
    fn scripted_select_extend(&mut self, handle: &str, click: SelectionClick) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(entity) = resolve_display_entity(world, handle) else {
            warn!(
                target: telemetry_target::ENGINE,
                handle = %handle,
                "[editor] select entity target not resolvable"
            );
            return;
        };
        // The Entities list's row order, which a range click reads.
        let rows: Vec<Entity> = world
            .get_resource::<EditorSnapshot>()
            .map(|snapshot| snapshot.entities.iter().map(|row| row.entity).collect())
            .unwrap_or_default();
        let Some(selection) = world.get_resource::<EditorSelection>() else {
            return;
        };
        let entities = selection.clicked(click, entity, &rows);
        let asset = selection.asset.clone();
        let count = entities.len();
        let label = match click {
            SelectionClick::Reset if count <= 1 => "select entity",
            _ => "select entities",
        };
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push_recorded(
                vec![EditorCommand::SetSelection { entities, asset }],
                label,
                Some("editor.selection".to_string()),
            );
        }
        info!(
            target: telemetry_target::ENGINE,
            entity = %entity,
            count,
            "[editor] selection changed"
        );
    }

    /// Log the selected entities, primary last (`dump selection`).
    fn dump_selection(&mut self) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let (entities, asset) = world
            .get_resource::<EditorSelection>()
            .map(|selection| (selection.entities().to_vec(), selection.asset.clone()))
            .unwrap_or_default();
        info!(
            target: telemetry_target::ENGINE,
            count = entities.len(),
            asset = asset.as_deref().unwrap_or("none"),
            primary = entities
                .last()
                .map(|entity| entity.to_string())
                .unwrap_or_else(|| "none".to_string())
                .as_str(),
            "[test] selection"
        );
        for entity in &entities {
            info!(
                target: telemetry_target::ENGINE,
                entity = %entity,
                "[test] selected entity"
            );
        }
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
        // A view of the selection for the shared asset helpers: only the asset
        // half is theirs, and the entity set is empty because nothing here
        // acts on entities.
        let mut selection = EditorSelection::default();
        selection.asset = selected;
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
                    overwrite: None,
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

    /// One scripted whole-field edit over the whole selection (`edit selection
    /// <component> <field> <value>`): the recorded batch an Inspector row
    /// queues when several entities are selected.
    ///
    /// The value parses against the primary's field, and the write reaches
    /// every selected entity whose detail carries the component, so an
    /// `EditorCommand::SetField` step is recorded once for the set and undoes
    /// in one step.
    fn scripted_edit_selection(&mut self, component_text: &str, field: &str, value_text: &str) {
        let (primary, details) = {
            let world = self.host.host_mut().engine_mut().world_mut();
            let Some(primary) = world
                .get_resource::<EditorSelection>()
                .and_then(|selection| selection.primary())
            else {
                warn!(
                    target: telemetry_target::ENGINE,
                    "[editor] selection edit needs a selected entity"
                );
                return;
            };
            let details = world
                .get_resource::<EditorSnapshot>()
                .map(|snapshot| snapshot.selected_details.clone())
                .unwrap_or_default();
            (primary, details)
        };
        let Some((entity, component, current)) =
            self.resolve_edit_target(&primary.to_string(), component_text, field)
        else {
            return;
        };
        let targets = inspector::selection_targets(&details, entity, &component);
        // An asset reference: the same recorded step for the whole selection,
        // resolved where it is applied.
        if matches!(current, FieldValue::Asset { .. }) {
            let reference = (!inspector::is_cleared_reference(value_text))
                .then(|| value_text.trim().to_owned());
            let (commands, label, coalesce) =
                inspector::selection_asset_edit(&targets, entity, &component, field, reference);
            let written = commands.len();
            let world = self.host.host_mut().engine_mut().world_mut();
            if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
                pending.push_recorded(commands, label, Some(coalesce));
            }
            info!(
                target: telemetry_target::ENGINE,
                component = component.as_str(),
                field,
                entities = written,
                "[editor] selection field edit queued"
            );
            return;
        }
        // An entity reference: the value is a Hierarchy label (`3v0`) or the
        // word that clears it, and the step is one recorded batch for the set.
        if matches!(current, FieldValue::Entity { .. }) {
            let reference = match inspector::parse_entity_reference(value_text) {
                Ok(reference) => reference,
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
            let (commands, label, coalesce) =
                inspector::selection_entity_edit(&targets, entity, &component, field, reference);
            let written = commands.len();
            let world = self.host.host_mut().engine_mut().world_mut();
            if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
                pending.push_recorded(commands, label, Some(coalesce));
            }
            info!(
                target: telemetry_target::ENGINE,
                component = component.as_str(),
                field,
                entities = written,
                "[editor] selection field edit queued"
            );
            return;
        }
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
        let (commands, label, coalesce) =
            inspector::selection_field_edit(&targets, entity, &component, field, value);
        let written = commands.len();
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push_recorded(commands, label, Some(coalesce));
        }
        info!(
            target: telemetry_target::ENGINE,
            component = component.as_str(),
            field,
            entities = written,
            "[editor] selection field edit queued"
        );
    }

    /// Arm an entity reference row (`pick entity <entity> <component>
    /// <field>`): the same `ArmEntityPick` the row's target button queues, so
    /// the next scene press names the field's reference.
    fn scripted_pick_entity(&mut self, entity_text: &str, component_text: &str, field: &str) {
        let Some((entity, component, current)) =
            self.resolve_edit_target(entity_text, component_text, field)
        else {
            return;
        };
        if !matches!(current, FieldValue::Entity { .. }) {
            warn!(
                target: telemetry_target::ENGINE,
                field,
                "[editor] inspector value not understood"
            );
            return;
        }
        let command = EditorCommand::ArmEntityPick {
            entity,
            component,
            field: field.to_owned(),
        };
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push(vec![command]);
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
        // An asset reference takes a path or a guid (or `none`), resolved -
        // and loaded when it is not loaded yet - where the step is applied.
        if matches!(current, FieldValue::Asset { .. }) {
            let reference = (!inspector::is_cleared_reference(value_text))
                .then(|| value_text.trim().to_owned());
            let (edit, label) = inspector::asset_edit(entity, &component, field, reference);
            let coalesce = inspector::field_coalesce_key(entity, &component, field);
            let world = self.host.host_mut().engine_mut().world_mut();
            if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
                pending.push_recorded(vec![edit], label, Some(coalesce));
            }
            return;
        }
        // An entity reference takes a Hierarchy label (`3v0`) or the word that
        // clears it.
        if matches!(current, FieldValue::Entity { .. }) {
            let reference = match inspector::parse_entity_reference(value_text) {
                Ok(reference) => reference,
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
            let (edit, label) = inspector::entity_edit(entity, &component, field, reference);
            let coalesce = inspector::field_coalesce_key(entity, &component, field);
            let world = self.host.host_mut().engine_mut().world_mut();
            if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
                pending.push_recorded(vec![edit], label, Some(coalesce));
            }
            return;
        }
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
        // A leaf the document does not hold yet reads as text: that is the
        // unbound-slot case, and the picker's value is a guid string.
        let kind = inspector::settings_field(&document, &key_path)
            .cloned()
            .unwrap_or_else(|| serde_json::Value::String(String::new()));
        let value = match inspector::parse_settings_value(value_text, &kind) {
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
        // The same inserting rule the Inspector's settings rows use.
        if let Err(message) = inspector::patch_settings_inserting(&mut patched, &key_path, value) {
            warn!(
                target: telemetry_target::ENGINE,
                key,
                message = message.as_str(),
                "[editor] asset settings edit refused"
            );
            return;
        }
        self.queue_settings_value(path, key, patched);
    }

    /// Queue the recorded step that writes `patched` over `path`'s settings.
    fn queue_settings_value(&mut self, path: &str, key: &str, patched: serde_json::Value) {
        let (edit, label) = inspector::settings_edit(path, patched, key);
        let coalesce = inspector::settings_coalesce_key(path, key);
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push_recorded(vec![edit], label, Some(coalesce));
        }
    }

    /// Queue the edit a picker row's choice would make: the chosen guid in the
    /// shape the field's document uses.
    ///
    /// The shape is the row's own answer - the object form for a named
    /// reference, a bare guid for a texture slot - so a scripted pick and a
    /// click cannot write different documents.
    fn pick_reference(&mut self, path: &str, key: &str, guid: &str) {
        let Some(document) = self.read_asset_settings(path) else {
            warn!(
                target: telemetry_target::ENGINE,
                path,
                "[editor] asset settings edit refused"
            );
            return;
        };
        let kind = self
            .asset_entries
            .iter()
            .find(|row| row.path == path || row.key() == path)
            .map(|row| row.kind.clone())
            .unwrap_or_default();
        let nested = self
            .asset_reference_fields(&kind, &Some(document.clone()))
            .into_iter()
            .find(|field| field.key == key)
            .map(|field| field.nested)
            // A slot the shader has not declared yet (its shader reference is
            // still another one) still writes the texture-slot shape, so the
            // document does not gain an object a renderer would not read.
            .unwrap_or_else(|| !key.starts_with("textures."));
        let value = if nested {
            serde_json::json!({ "guid": guid })
        } else {
            serde_json::Value::String(guid.to_owned())
        };
        let Ok(key_path) = inspector::parse_settings_key_path(key) else {
            warn!(
                target: telemetry_target::ENGINE,
                key,
                "[editor] asset settings edit refused"
            );
            return;
        };
        let mut patched = document;
        if let Err(message) = inspector::patch_settings_inserting(&mut patched, &key_path, value) {
            warn!(
                target: telemetry_target::ENGINE,
                key,
                message = message.as_str(),
                "[editor] asset settings edit refused"
            );
            return;
        }
        self.queue_settings_value(path, key, patched);
    }

    /// Run one world frame: input, the shell's pass, the overlay draw.
    ///
    /// Returns whether the run should end. The event loop is what a scene
    /// view the dock opened this frame needs: a child window is created
    /// through it.
    fn frame(&mut self, event_loop: &ActiveEventLoop) -> Result<bool, EditorError> {
        self.frame_index += 1;
        // Step 1: the frame rate heartbeat, for the person reading the log.
        let frame_started = Instant::now();
        let now = frame_started;
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
            let idle_frame_millis = self.idle_frame_millis_max;
            self.idle_frame_millis_max = 0.0;
            info!(
                target: telemetry_target::ENGINE,
                fps = self.fps,
                frame = self.frame_index,
                overlay_frames,
                idle_frame_millis,
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
        // and Problems rows from.
        self.fold_host_notices();
        // Step 2c2: where each entity lands in each visible scene view, for
        // the shell's badges.
        self.publish_badges();
        // Step 2d: the document's title is reported when it moves, so the log
        // names the scene the session holds; the window's own title names the
        // project.
        self.report_scene_title();
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
            // A paste chord in this frame's events carries the OS clipboard's
            // text into egui, so a focused text field pastes.
            let paste_text = paste_chord(&events).then(|| {
                crate::clipboard::read()
                    .ok()
                    .flatten()
                    .filter(|text| !text.is_empty())
            });
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
                paste_text: paste_text.flatten(),
                badges: self.badges.clone(),
                // The row geometry tokens exist for the scripted channel; a
                // person's editor logs none of them.
                scripted: self.test_channel.is_some(),
            };
        }
        // Step 3a2: each popped-out window's own input record: its geometry,
        // focus and frame rate this frame. Its events are appended as its
        // window reports them and cleared when the frame that read them is
        // presented, so a record never replays an event.
        self.publish_popout_geometry();
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
        self.watch_theme();
        self.watch_keybindings();
        self.publish_console_lines();
        self.drain_problem_open();
        // Step 3d: the panels the loaded modules asked to register, then what
        // the panels drawn last frame asked for. The sync is
        // before the snapshot so a tab's title is never a frame stale.
        // The recovery document a killed run left behind is applied before the
        // dock is read, and the current state is written
        // back once its arrangement or selection changed.
        self.apply_recovered_state();
        self.autosave_recovery();
        self.sync_panels();
        self.apply_panel_commands();
        // The clipboard the shell queued last frame: the OS
        // half is this executable's, applied here like every other queued
        // request.
        self.apply_clipboard();
        // Step 4: the frame's snapshot for the shell's data tabs, when the
        // shell loaded and registered its resources.
        self.publish_snapshot();
        // Step 5: the world resources the frame draws and picks with,
        // published before it runs: the shared selection's outline set (one
        // write per change) and the gizmo handles for it.
        self.publish_selection_outline();
        self.publish_gizmo_geometry();
        // Step 6: run the world frame; the shell's egui pass runs inside it
        // (a registered post-update system) and leaves the frame resource
        // for Step 8.
        let report = self.host.run_one_frame().map_err(EditorError::Renderer)?;
        let _ = report;
        // Step 7: answers to scene picks that arrived during the frame
        // move the shared selection; the outline follows on the next
        // publish.
        self.drain_scene_picks();
        // Step 7a: the asset previews the renderer read back, and the next one
        // to ask for.
        self.apply_asset_previews();
        // Step 7b: the menu bar's requests - undo/redo, a window switch and a
        // quit - taken through the same paths the scripted channel uses.
        self.drain_menu_requests();
        // Step 8: draw the shell frame through the overlay.
        self.present_ui(event_loop);
        // Step 9: the scripted channel, then the auto-quit deadline.
        self.poll_test_channel();
        if let Some(seconds) = self.test_seconds {
            if self.started.elapsed().as_secs_f64() >= seconds {
                info!(
                    target: telemetry_target::ENGINE,
                    "[editor] test deadline reached; quitting"
                );
                self.request_quit();
            }
        }
        // Step 10: what the loop does next - draw again at
        // once when anything asked for a frame, or sleep for the idle poll
        // and let this frame's evidence expire.
        let frame_millis = frame_started.elapsed().as_secs_f32() * 1000.0;
        if self.polling {
            self.idle_frame_millis_max = self.idle_frame_millis_max.max(frame_millis);
        }
        self.decide_next_wake(frame_millis);
        Ok(self.quit)
    }

    /// Decide when the loop draws next, from what asked for this frame.
    ///
    /// The demand is cleared here rather than at the start of the next frame:
    /// an event that arrives between the two sets it again, and the loop
    /// already knows to draw at once. A frame that leaves the loop polling
    /// logs `[editor] frame idle` once per transition, with the cost of the
    /// frame that settled.
    fn decide_next_wake(&mut self, frame_millis: f32) {
        let demand = FrameDemand {
            animating: self.animating(),
            repaint_requested: self.repaint_after.is_some(),
            ..self.demand
        };
        self.cadence.record_frame(demand);
        self.last_demand = demand;
        self.next_wake = self.cadence.decide(demand);
        let polling = matches!(self.next_wake, FrameWake::After(_));
        if polling {
            self.cadence.record_wait();
        }
        if polling && !self.polling {
            info!(
                target: telemetry_target::ENGINE,
                poll_millis = pill_editor_core::cadence::IDLE_POLL.as_millis() as u64,
                frame_millis,
                "[editor] frame idle"
            );
        }
        self.polling = polling;
        self.demand = FrameDemand::default();
        self.repaint_after = None;
    }

    /// Whether something is moving, so the next frame is already owed.
    ///
    /// A paused project stops animating; a camera navigation and a scripted
    /// drag still move something, and a toast's countdown changes the frame's
    /// pixels as it expires.
    fn animating(&self) -> bool {
        let running = !self.host.host().engine().systems_paused();
        running || self.camera_drag.is_some() || self.scripted_drag.is_some()
    }

    /// The wake the last frame left for the loop.
    fn next_wake(&self) -> FrameWake {
        self.next_wake
    }

    /// Drain the module panels' registration requests.
    ///
    /// A request names a draw function that lives in the requesting module's
    /// image, so it is bound here, in the executable, to that module's
    /// liveness marker: the registry hands the function out only while the
    /// marker is alive, and a reload drops the marker (see
    /// [`Self::fold_host_notices`]).
    fn sync_panels(&mut self) {
        let requests = {
            let world = self.host.host_mut().engine_mut().world_mut();
            world
                .get_resource_mut::<PanelRegistrations>()
                .map(|registrations| registrations.drain())
                .unwrap_or_default()
        };
        if requests.is_empty() {
            return;
        }
        for request in requests {
            let marker = self
                .panel_markers
                .entry(request.source.to_string())
                .or_default();
            let slot = {
                let world = self.host.host_mut().engine_mut().world_mut();
                match world.get_resource_mut::<PanelRegistry>() {
                    Some(registry) => registry.register(
                        request.id,
                        request.title,
                        request.source,
                        request.draw,
                        marker,
                    ),
                    None => continue,
                }
            };
            info!(
                target: telemetry_target::ENGINE,
                id = request.id,
                slot,
                source = request.source,
                "[editor] panel registered"
            );
            match self
                .panel_requests
                .iter_mut()
                .find(|known| known.id == request.id)
            {
                Some(known) => *known = request,
                None => self.panel_requests.push(request),
            }
        }
    }

    /// Queue the component-field write a module panel's own row would queue
    /// (`panel field <component> <field> <value>`).
    ///
    /// The row is a drag value the test channel cannot press, so the write is
    /// injected where the row puts it: the shared `PanelCommands` queue, applied
    /// by [`Self::apply_panel_commands`] at the next frame boundary. What a test
    /// drives is therefore the panel contract itself - the queue, the recorded
    /// field edit and its undo step - not a second write path.
    fn scripted_panel_field(&mut self, component: &str, field: &str, value: &str) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(entity) = world
            .get_resource::<EditorSelection>()
            .and_then(|selection| selection.primary())
        else {
            warn!(
                target: telemetry_target::ENGINE,
                component,
                field,
                "[editor] panel field needs a selected entity"
            );
            return;
        };
        let mut queued = false;
        if let Some(commands) = world.get_resource_mut::<PanelCommands>() {
            commands.push(PanelCommand::SetField {
                entity,
                component: component.to_string(),
                field: field.to_string(),
                value: value.to_string(),
            });
            queued = true;
        }
        if queued {
            info!(
                target: telemetry_target::ENGINE,
                entity = %entity,
                component,
                field,
                value,
                "[editor] panel field queued"
            );
        }
    }

    /// One scripted clipboard command (`command editor.copy/cut/paste`): the
    /// same request the Edit menu's rows queue, so the test channel drives the
    /// menu's own path.
    fn scripted_clipboard(&mut self, action: ClipboardAction) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let selection = world
            .get_resource::<EditorSelection>()
            .cloned()
            .unwrap_or_default();
        let snapshot = world
            .get_resource::<EditorSnapshot>()
            .cloned()
            .unwrap_or_default();
        let summary = match world.get_resource_mut::<EditorClipboard>() {
            Some(clipboard) => clipboard::queue_request(clipboard, action, &selection, &snapshot),
            None => None,
        };
        // The cut's delete is queued from its own borrow: the two resources
        // cannot be held mutably at once.
        if action == ClipboardAction::Cut {
            if let (Some((commands, label)), Some(pending)) = (
                clipboard::cut_commands(&selection),
                world.get_resource_mut::<PendingEditorCommands>(),
            ) {
                pending.push_recorded(commands, label, None);
            }
        }
        info!(
            target: telemetry_target::ENGINE,
            action = match action {
                ClipboardAction::Copy => "copy",
                ClipboardAction::Cut => "cut",
                ClipboardAction::Paste => "paste",
            },
            payload = summary.as_deref().unwrap_or("nothing"),
            "[editor] clipboard queued"
        );
    }

    /// Carry out the clipboard request the shell queued.
    ///
    /// A write puts the payload on the OS clipboard; a read decodes what it
    /// carries and applies the paste as one recorded step. What could not be
    /// pasted - an unregistered component, a field this build no longer
    /// carries - is named in a notice, because a paste that quietly dropped
    /// half an entity is worse than one that says what it left behind.
    fn apply_clipboard(&mut self) {
        let request = {
            let world = self.host.host_mut().engine_mut().world_mut();
            world
                .get_resource_mut::<EditorClipboard>()
                .and_then(|clipboard| clipboard.take_request())
        };
        let Some(request) = request else {
            return;
        };
        match request {
            ClipboardRequest::Write(text) => match crate::clipboard::write(&text) {
                Ok(()) => info!(
                    target: telemetry_target::ENGINE,
                    bytes = text.len(),
                    writer = crate::clipboard::process_id(),
                    "[editor] clipboard written"
                ),
                Err(message) => {
                    warn!(
                        target: telemetry_target::ENGINE,
                        message = message.as_str(),
                        "[editor] clipboard write failed"
                    );
                    self.push_toast(NoticeLevel::Warning, format!("clipboard: {message}"));
                }
            },
            ClipboardRequest::Read => self.apply_paste(),
        }
    }

    /// Read the OS clipboard and apply what it carries, as one recorded step.
    fn apply_paste(&mut self) {
        let text = match crate::clipboard::read() {
            Ok(Some(text)) => text,
            Ok(None) => {
                self.push_toast(
                    NoticeLevel::Warning,
                    "clipboard: the clipboard is empty".to_string(),
                );
                return;
            }
            Err(message) => {
                warn!(
                    target: telemetry_target::ENGINE,
                    message = message.as_str(),
                    "[editor] clipboard read failed"
                );
                self.push_toast(NoticeLevel::Warning, format!("clipboard: {message}"));
                return;
            }
        };
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(clipboard) = world.get_resource_mut::<EditorClipboard>() {
            clipboard.text = text.clone();
        }
        let payload = match clipboard::decode(&text) {
            Ok(payload) => payload,
            Err(message) => {
                warn!(
                    target: telemetry_target::ENGINE,
                    message = message.as_str(),
                    "[editor] clipboard payload refused"
                );
                self.push_toast(NoticeLevel::Warning, format!("clipboard: {message}"));
                return;
            }
        };
        let selection = world
            .get_resource::<EditorSelection>()
            .cloned()
            .unwrap_or_default();
        let snapshot = world
            .get_resource::<EditorSnapshot>()
            .cloned()
            .unwrap_or_default();
        let plan = match clipboard::paste_plan(&payload, &selection, &snapshot) {
            Ok(plan) => plan,
            Err(message) => {
                warn!(
                    target: telemetry_target::ENGINE,
                    message = message.as_str(),
                    "[editor] clipboard paste refused"
                );
                self.push_toast(NoticeLevel::Warning, format!("clipboard: {message}"));
                return;
            }
        };
        if !plan.commands.is_empty() {
            if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
                pending.push_recorded(plan.commands, plan.label.clone(), None);
            }
        }
        if !plan.note.is_empty() {
            self.push_toast(NoticeLevel::Warning, format!("clipboard: {}", plan.note));
        }
        info!(
            target: telemetry_target::ENGINE,
            kind = payload.kind(),
            label = plan.label.as_str(),
            skipped = plan.skipped.len(),
            reader = crate::clipboard::process_id(),
            "[editor] clipboard pasted"
        );
        if !plan.skipped.is_empty() {
            let message = format!("skipped {}", plan.skipped.join(", "));
            self.push_toast(NoticeLevel::Warning, format!("clipboard: {message}"));
        }
    }

    /// Apply what the module panels asked for.
    ///
    /// The same contract `EditorCommand` follows: a panel queues a request, the
    /// executable applies it at the frame boundary, so a panel from a module
    /// never leaves the world half-edited.
    fn apply_panel_commands(&mut self) {
        let commands = {
            let world = self.host.host_mut().engine_mut().world_mut();
            world
                .get_resource_mut::<PanelCommands>()
                .map(|queue| queue.drain())
                .unwrap_or_default()
        };
        for command in commands {
            match command {
                PanelCommand::SelectEntity(entity) => {
                    let world = self.host.host_mut().engine_mut().world_mut();
                    if let Some(selection) = world.get_resource_mut::<EditorSelection>() {
                        selection.select_only(entity);
                    }
                    info!(
                        target: telemetry_target::ENGINE,
                        entity = %entity,
                        "[editor] panel selected an entity"
                    );
                }
                // The recorded edit path every inspector row takes, so a
                // panel's write is one undo step like any other.
                PanelCommand::SetField {
                    entity,
                    component,
                    field,
                    value,
                } => self.panel_field_write(entity, &component, &field, &value),
                PanelCommand::SetArrayElement {
                    entity,
                    component,
                    field,
                    index,
                    value,
                } => self.panel_element_write(entity, &component, &field, index, &value),
                // The windows belong to the shell, which applies these while
                // it draws; the executable has nothing to do with them.
                PanelCommand::OpenWindow(_) | PanelCommand::CloseWindow(_) => {}
                PanelCommand::Log(text) => info!(
                    target: telemetry_target::ENGINE,
                    text = text.as_str(),
                    "[editor] panel log"
                ),
            }
        }
    }

    /// Write one component field from a panel, recorded.
    fn panel_field_write(
        &mut self,
        entity: Entity,
        component: &str,
        field: &str,
        value_text: &str,
    ) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(component) = resolve_component_name(world, component) else {
            warn!(
                target: telemetry_target::ENGINE,
                component,
                "[editor] panel field write: no such component"
            );
            return;
        };
        let current = match world.read_component_field(entity, &component, field) {
            Ok(current) => current,
            Err(_) => {
                warn!(
                    target: telemetry_target::ENGINE,
                    component = component.as_str(),
                    field,
                    "[editor] panel field write: no such field"
                );
                return;
            }
        };
        let value = match inspector::parse_field_value(value_text, &current) {
            Ok(value) => value,
            Err(message) => {
                warn!(
                    target: telemetry_target::ENGINE,
                    field,
                    message = message.as_str(),
                    "[editor] panel field write: value not understood"
                );
                return;
            }
        };
        let (edit, label) = inspector::field_edit(entity, &component, field, value);
        let coalesce = inspector::field_coalesce_key(entity, &component, field);
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push_recorded(vec![edit], label, Some(coalesce));
        }
        info!(
            target: telemetry_target::ENGINE,
            component = component.as_str(),
            field,
            value = value_text,
            "[editor] panel wrote a field"
        );
    }

    /// Write one array element from a panel, recorded.
    fn panel_element_write(
        &mut self,
        entity: Entity,
        component: &str,
        field: &str,
        index: usize,
        value_text: &str,
    ) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(component) = resolve_component_name(world, component) else {
            return;
        };
        let Ok(current) = world.read_component_field(entity, &component, field) else {
            warn!(
                target: telemetry_target::ENGINE,
                component = component.as_str(),
                field,
                "[editor] panel element write: no such field"
            );
            return;
        };
        let Some(element) = inspector::element_field(&current, index) else {
            warn!(
                target: telemetry_target::ENGINE,
                field,
                index,
                "[editor] panel element write: no such element"
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
                    "[editor] panel element write: value not understood"
                );
                return;
            }
        };
        let (edit, label) = inspector::element_edit(entity, &component, field, index, value);
        let coalesce = inspector::field_coalesce_key(entity, &component, field);
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push_recorded(vec![edit], label, Some(coalesce));
        }
        info!(
            target: telemetry_target::ENGINE,
            component = component.as_str(),
            field,
            index,
            value = value_text,
            "[editor] panel wrote an array element"
        );
    }

    /// Capture the editor snapshot the shell's tabs read, when the shell
    /// loaded and registered it.
    ///
    /// The asset listing refreshes on its cadence - or at once after a file
    /// command - because the walk touches the filesystem; the detail tier,
    /// the Inspector's data, is rebuilt for the current selection every
    /// frame: it is one entity's components, cheap, and it keeps the tab's
    /// values live.
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
            .and_then(|selection| selection.primary());
        let selected_entities: Vec<Entity> = engine
            .world()
            .get_resource::<EditorSelection>()
            .map(|selection| selection.entities().to_vec())
            .unwrap_or_default();
        let mut snapshot = EditorSnapshot::capture_list(engine, self.frame_index, &[], Vec::new());
        snapshot.detail =
            selected.and_then(|entity| EditorSnapshot::capture_detail(engine, entity));
        // Every selected entity's detail: a shared-field edit
        // writes only the entities a component exists on, and a row's mixed
        // marker reads them.
        snapshot.selected_details = selected_entities
            .iter()
            .take(pill_editor_core::editor_state::SELECTED_DETAILS_MAX)
            .filter_map(|entity| EditorSnapshot::capture_detail(engine, *entity))
            .collect();
        snapshot.assets = self.asset_entries.clone();
        snapshot.engine_assets = engine_asset_rows(engine.world(), &self.asset_entries);
        snapshot.standalone_types = self.standalone_types.clone();
        snapshot.asset_detail = self.asset_detail.clone();
        snapshot.preferences = PreferencesView::from_document(
            &self.preferences_document,
            self.gizmo_project.as_deref(),
        );
        // The theme the shell installs: the executable is
        // the side that owns the layers, so the merge travels on the snapshot
        // with the generation that tells the shell whether to install it.
        snapshot.theme = self.theme.clone();
        snapshot.theme_generation = self.theme_generation;
        snapshot.theme_layers = self.theme_layers.clone();
        snapshot.theme_fonts = self.theme_fonts.clone();
        let (undo_depth, redo_depth) = engine
            .world()
            .get_resource::<CommandHistory>()
            .map(|history| (history.undo_depth(), history.redo_depth()))
            .unwrap_or((0, 0));
        snapshot.undo_depth = undo_depth;
        snapshot.redo_depth = redo_depth;
        // The redraw cadence's change detector: this frame's capture is
        // compared field by field against the snapshot resource in place. The
        // frame index is not compared, since it is this frame's, and neither
        // are the two tiers a frame itself feeds: its own log lines reach the
        // console, so treating them as a reason to draw would make every
        // frame owe the next one. They are bounded-latency data instead - the
        // idle poll draws them within its interval - which is what keeps an
        // idle editor idle while its console still fills.
        let fields = engine
            .world()
            .get_resource::<EditorSnapshot>()
            .map(|previous| snapshot_fields_changed(previous, &snapshot))
            .unwrap_or_default();
        self.demand.snapshot = !fields.is_empty();
        // Which fields moved is the answer to "why is the editor drawing every
        // frame?"; one line per new combination keeps the log honest without
        // streaming.
        if self.demand.snapshot && self.snapshot_changed_field.as_deref() != Some(fields.as_str()) {
            self.snapshot_changed_field = Some(fields.clone());
            info!(
                target: telemetry_target::ENGINE,
                fields = fields.as_str(),
                "[editor] snapshot changed"
            );
        }
        if let Some(resource) = engine.world_mut().get_resource_mut::<EditorSnapshot>() {
            *resource = snapshot;
        }
    }

    /// Refresh the asset browser's rows when the cadence is due or a file
    /// command marked them dirty, and pick up the live standalone types the
    /// create dialog offers. Returns whether the walk ran, so the asset detail
    /// follows it.
    fn refresh_asset_listing(&mut self) -> bool {
        if !(self.frame_index.is_multiple_of(ASSET_REFRESH_FRAMES) || self.asset_listing_dirty) {
            return false;
        }
        // An edit asked for this pass, not the cadence: settings or a file
        // changed, so the tiles' images and the Inspector's capture both have
        // to follow.
        let edited = self.asset_listing_dirty;
        self.asset_listing_dirty = false;
        let entries: Vec<AssetListing> = self
            .host
            .host_mut()
            .asset_entries()
            .into_iter()
            .take(ASSET_ROWS_MAX)
            .map(|entry| AssetListing {
                root: entry.root,
                path: entry.path,
                read_only: entry.read_only,
                kind: if entry.is_directory {
                    "folder".to_string()
                } else {
                    entry.type_name.unwrap_or_else(|| "file".to_string())
                },
                standalone: entry.standalone,
                has_meta: entry.guid.is_some(),
                guid: entry.guid.map(|guid| guid.to_string()),
            })
            .collect();
        // The walk runs on a cadence, so most of its passes find the tree
        // exactly as it was. Only a moved tree - or an edit - makes a tile's
        // image stale; clearing the previews on every pass is what made them
        // blink back to their icons every couple of seconds.
        let tree_moved = entries != self.asset_walk;
        // A tree that moved keeps the images of the rows that did not: a
        // create or a delete changes one asset, and re-rendering the folder
        // for it is the same flicker in a smaller place. An edit that left the
        // tree alone names no asset, so every image may be stale.
        let unchanged: std::collections::BTreeSet<String> = if tree_moved {
            entries
                .iter()
                .filter(|row| {
                    self.asset_walk
                        .iter()
                        .any(|old| old.key() == row.key() && *old == **row)
                })
                .map(AssetListing::key)
                .collect()
        } else {
            std::collections::BTreeSet::new()
        };
        self.asset_walk = entries.clone();
        self.asset_entries = entries;
        if tree_moved || edited {
            let world = self.host.host_mut().engine_mut().world_mut();
            if let Some(state) = world.get_resource_mut::<EguiShellState>() {
                if tree_moved {
                    state.previews.retain(|key, _| unchanged.contains(key));
                    state.preview_refused.retain(|key| unchanged.contains(key));
                } else {
                    self.preview_in_flight.clear();
                    state.previews.clear();
                    state.preview_refused.clear();
                }
            }
        }
        // Every module root needs its own row under the extensions root: the
        // walk lists what is inside a root, and the tree can only open a folder
        // it has a row for. The root itself is not a row - the panel draws it.
        let roots: std::collections::BTreeSet<String> = self
            .asset_entries
            .iter()
            .map(|row| row.root.clone())
            .filter(|root| !root.is_empty())
            .collect();
        for root in roots.into_iter().rev() {
            self.asset_entries.insert(
                0,
                AssetListing {
                    root,
                    path: String::new(),
                    read_only: true,
                    kind: "folder".to_string(),
                    standalone: false,
                    has_meta: false,
                    guid: None,
                },
            );
        }
        let types = pill_host::standalone_types(self.host.host_mut().engine_mut().world());
        self.standalone_types = types
            .into_iter()
            .map(|info| StandaloneTypeInfo {
                type_name: info.type_name,
                display_name: info.display_name,
                extension: info.extension,
            })
            .collect();
        let changed = tree_moved || edited;
        if changed {
            info!(
                target: telemetry_target::ENGINE,
                count = self.asset_entries.len(),
                types = self.standalone_types.len(),
                "[editor] asset listing refreshed"
            );
        }
        changed
    }

    /// Capture the selected asset's detail for the Inspector's asset
    /// section, when the selection or the tree changed: reading the settings
    /// document is a disk read, so the capture follows the listing's cadence
    /// instead of running every frame. A settings write forgets the captured
    /// path, so the widgets see the document the command wrote.
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
        self.asset_detail = selected.map(|key| {
            let row = self.asset_entries.iter().find(|row| row.key() == key);
            let kind = row
                .map(|row| row.kind.clone())
                .unwrap_or_else(|| "file".to_string());
            let standalone = row.map(|row| row.standalone).unwrap_or(false);
            let root = row.map(|row| row.root.clone()).unwrap_or_default();
            let read_only = row.map(|row| row.read_only).unwrap_or(false);
            // The settings document is read by the path inside the root: the
            // store searches the module roots, so a module resource reads
            // like a project asset.
            let path = row
                .map(|row| row.path.clone())
                .unwrap_or_else(|| key.clone());
            let settings = if row.is_some_and(|row| row.kind == "folder") {
                None
            } else {
                self.read_asset_settings(&path)
            };
            let references = self.asset_reference_fields(&kind, &settings);
            AssetDetail {
                root,
                path,
                read_only,
                kind,
                standalone,
                settings,
                references,
            }
        });
    }

    /// The reference fields the asset's type declares, expanded against its
    /// document: one entry per picker the Inspector draws, with the guid the
    /// document holds now.
    ///
    /// The registry names the fields and the type each points at - `shader`,
    /// `textures.*`, `passes.*` - and the families are expanded here, because
    /// only the document knows how many entries a family has: a material's
    /// texture slots come from the shader beside it, a chain's entries from its
    /// own list.
    fn asset_reference_fields(
        &mut self,
        kind: &str,
        settings: &Option<serde_json::Value>,
    ) -> Vec<AssetReferenceField> {
        let Some(settings) = settings.as_ref() else {
            return Vec::new();
        };
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(registry) = world
            .get_resource::<pill_engine_core::ImportRegistry>()
            .cloned()
        else {
            return Vec::new();
        };
        let declared = registry.reference_fields(kind).to_vec();
        let mut fields = Vec::new();
        for (key, target) in declared {
            let Some(family) = key.strip_suffix(".*") else {
                let value = settings.get(&key);
                let guid = reference_guid(value);
                fields.push(AssetReferenceField {
                    key,
                    target,
                    label: self.asset_label(&guid),
                    guid,
                    nested: !matches!(value, Some(serde_json::Value::String(_))),
                });
                continue;
            };
            match family {
                "textures" => {
                    for slot in self.shader_texture_slots(&Some(settings.clone())) {
                        let value = settings.get("textures").and_then(|map| map.get(&slot));
                        let guid = reference_guid(value);
                        fields.push(AssetReferenceField {
                            key: format!("textures.{slot}"),
                            target: target.clone(),
                            label: self.asset_label(&guid),
                            guid,
                            nested: false,
                        });
                    }
                }
                "passes" => {
                    let entries = settings
                        .get(family)
                        .and_then(|list| list.as_array())
                        .cloned()
                        .unwrap_or_default();
                    for (index, entry) in entries.iter().enumerate() {
                        let guid = reference_guid(Some(entry));
                        fields.push(AssetReferenceField {
                            key: format!("passes[{index}]"),
                            target: target.clone(),
                            label: self.asset_label(&guid),
                            guid,
                            nested: true,
                        });
                    }
                }
                other => debug_assert!(false, "unexpanded reference family `{other}`"),
            }
        }
        fields
    }

    /// What a reference row reads for the asset `guid` names: the listing's
    /// path when the file is one, otherwise the name the store holds - a
    /// chain's built-in passes are built in code, so their guid is the only
    /// thing a file walk can know about them.
    fn asset_label(&mut self, guid: &Option<String>) -> Option<String> {
        let guid = guid.as_deref()?;
        if let Some(path) = self
            .asset_entries
            .iter()
            .find(|row| row.guid.as_deref() == Some(guid))
            .map(|row| row.path.clone())
        {
            return Some(path);
        }
        let parsed = pill_engine_core::AssetGuid::parse(guid)?;
        let world = self.host.host_mut().engine_mut().world_mut();
        let registry = world
            .get_resource::<pill_engine_core::ImportRegistry>()
            .cloned()?;
        let assets = world.get_resource::<pill_engine_core::AssetManager>()?;
        registry
            .loaded_by_guid(assets, parsed)
            .map(|(_type_name, name, _index, _generation)| name)
    }

    /// The texture slots the shader a material document names declares.
    ///
    /// A material's rows are the shader's slots: `RendererMaterial` builds its
    /// bind group from them, so the Inspector asks the same source. The shader
    /// is found through its guid, which is what the document holds, and its
    /// declarations are read from the `.hlsl.meta` the listing row points at.
    fn shader_texture_slots(&mut self, settings: &Option<serde_json::Value>) -> Vec<String> {
        let Some(reference) = settings
            .as_ref()
            .and_then(|document| document.get("shader"))
        else {
            return Vec::new();
        };
        let guid = match reference {
            serde_json::Value::String(guid) => guid.clone(),
            other => match other.get("guid").and_then(|guid| guid.as_str()) {
                Some(guid) => guid.to_owned(),
                None => return Vec::new(),
            },
        };
        let Some(path) = self
            .asset_entries
            .iter()
            .find(|row| row.guid.as_deref() == Some(guid.as_str()))
            .map(|row| row.path.clone())
        else {
            return Vec::new();
        };
        let Some(shader) = self.read_asset_settings(&path) else {
            return Vec::new();
        };
        shader
            .get("textures")
            .and_then(|slots| slots.as_array())
            .map(|slots| {
                slots
                    .iter()
                    .filter_map(|slot| slot.get("name")?.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
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
    /// ([`pill_editor_core::drain::apply_pending`]), so the browser
    /// editor runs exactly this contract; the only executable-owned half is
    /// the asset listing walk a file batch asks for and the reveal requests,
    /// which only this side can carry out.
    fn apply_pending_commands(&mut self) {
        self.apply_reveals();
        self.apply_focuses();
        let now_millis = self.started.elapsed().as_millis() as u64;
        let outcome = drain::apply_pending(self.host.host_mut().engine_mut(), now_millis);
        if outcome.files_changed {
            self.asset_listing_dirty = true;
        }
        if outcome.settings_changed {
            // The Inspector's rows draw the document the command just wrote;
            // forgetting the captured path is what makes the next frame read it
            // again, so a committed row shows its new value (and a reordered
            // list its new order) at once.
            self.asset_detail_path = None;
        }
    }

    /// Show every queued focus request: the selection moves to the asset and
    /// the Assets panel opens the folder that holds it.
    ///
    /// The shell cannot do either - the selection is a world resource and the
    /// open folder lives in the session it only borrows per pass - so the
    /// request crosses here and lands in one place, which is also what makes it
    /// one log line a suite can read.
    fn apply_focuses(&mut self) {
        let keys = {
            let world = self.host.host_mut().engine_mut().world_mut();
            world
                .get_resource_mut::<PendingEditorCommands>()
                .map(|pending| pending.take_focuses())
                .unwrap_or_default()
        };
        for key in keys {
            let world = self.host.host_mut().engine_mut().world_mut();
            if let Some(selection) = world.get_resource_mut::<EditorSelection>() {
                selection.asset = Some(key.clone());
            }
            if let Some(state) = world.get_resource_mut::<EguiShellState>() {
                let folder = key
                    .rsplit_once('/')
                    .map(|(parent, _)| parent.to_string())
                    .unwrap_or_default();
                state.browser.open(&folder);
            }
            info!(
                target: telemetry_target::ENGINE,
                path = key.as_str(),
                "[editor] asset focused in the panel"
            );
        }
    }

    /// Open the platform's file manager for every queued reveal.
    ///
    /// The shell knows the project-relative path; this side knows where the
    /// project lives and which file manager the OS has, so the request
    /// crosses as a path and the answer is an OS process. A refusal - no
    /// mounted directory, a path that left the root or no longer exists - is a
    /// console line, never a panic.
    fn apply_reveals(&mut self) {
        let paths = {
            let world = self.host.host_mut().engine_mut().world_mut();
            world
                .get_resource_mut::<PendingEditorCommands>()
                .map(|pending| pending.take_reveals())
                .unwrap_or_default()
        };
        if paths.is_empty() {
            return;
        }
        let Some(root) = self
            .host
            .host_mut()
            .asset_directory()
            .map(std::path::Path::to_path_buf)
        else {
            for path in paths {
                warn!(
                    target: telemetry_target::ENGINE,
                    path = path.as_str(),
                    "[editor] reveal refused: the host knows no asset directory"
                );
            }
            return;
        };
        // A module's resources live outside the project's tree, so a key under
        // the extensions root resolves against that module's own directory.
        let module_roots: Vec<(String, std::path::PathBuf)> =
            pill_engine_core::asset_store::module_roots()
                .into_iter()
                .map(|root| (root.module, root.directory))
                .collect();
        for path in paths {
            match assets::reveal_target_in(&root, &module_roots, &path) {
                Ok(target) => {
                    open_in_file_manager(&target);
                    info!(
                        target: telemetry_target::ENGINE,
                        path = path.as_str(),
                        target = %target.display(),
                        "[editor] reveal"
                    );
                }
                Err(message) => warn!(
                    target: telemetry_target::ENGINE,
                    path = path.as_str(),
                    message = message.as_str(),
                    "[editor] reveal refused"
                ),
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
    /// The file is only read when its metadata moved, and once per
    /// [`STATE_FILE_POLL_FRAMES`] regardless, so a coarse or unchanged
    /// timestamp still reaches the editor within a fraction of a second. The
    /// tool's mode, space and toggle stay whatever the world state holds, so
    /// the stale copy a persisting write leaves behind never moves the live
    /// tool; the snapping steps are the file's, one set for every project.
    fn watch_preferences(&mut self) {
        let directory = layout_directory();
        let path = directory.join(PREFERENCES_FILE);
        let stamp = FileStamp::of(&path);
        let due = self.frame_index.is_multiple_of(STATE_FILE_POLL_FRAMES);
        if !due && stamp == self.preferences_stamp {
            return;
        }
        self.preferences_stamp = stamp;
        let text = std::fs::read_to_string(&path).ok();
        if text == self.preferences_text {
            return;
        }
        self.preferences_text = text;
        let preferences = self
            .preferences_text
            .as_deref()
            .and_then(|text| serde_json::from_str::<EditorPreferences>(text).ok())
            .unwrap_or_default();
        self.preferences_document = match self.preferences_text.as_deref() {
            Some(text) => match preferences::parse_document(text) {
                Ok(document) => document,
                Err(message) => {
                    // A file the editor cannot parse is shown as the defaults
                    // and said out loud; folding it into an empty document
                    // silently is how a corrupt file goes unnoticed.
                    warn!(
                        target: telemetry_target::ENGINE,
                        error = message.as_str(),
                        "[editor] preferences refused"
                    );
                    serde_json::Map::new()
                }
            },
            None => serde_json::Map::new(),
        };
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
        // The gizmo's hit radius is a preference: the hover and the
        // press read this cached value, so one edit moves both.
        let hit_radius = preferences::gizmo_hit_radius(&self.preferences_document);
        if (hit_radius - self.gizmo_hit_radius).abs() > f32::EPSILON {
            self.gizmo_hit_radius = hit_radius;
            info!(
                target: telemetry_target::ENGINE,
                hit_radius,
                "[editor] gizmo hit radius"
            );
        }
        // Asset previews are the preference's to switch:
        // turning them off drops the images already made, so the tiles go back
        // to their icons rather than showing a stale picture.
        let render_previews = previews::render_previews(&self.preferences_document);
        if render_previews != self.render_previews {
            self.render_previews = render_previews;
            if !render_previews {
                self.preview_in_flight.clear();
                if let Some(state) = self
                    .host
                    .host_mut()
                    .engine_mut()
                    .world_mut()
                    .get_resource_mut::<EguiShellState>()
                {
                    state.previews.clear();
                    state.preview_refused.clear();
                }
            } else if let Some(state) = self
                .host
                .host_mut()
                .engine_mut()
                .world_mut()
                .get_resource_mut::<EguiShellState>()
            {
                // Turning previews on asks again for everything a refusal left
                // behind, so a tile refused while the switch was off does not
                // stay blank for the session.
                state.preview_refused.clear();
            }
            info!(
                target: telemetry_target::ENGINE,
                enabled = render_previews,
                "[editor] asset previews"
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

    /// Re-resolve the editor's theme layers and record a new generation when
    /// the result changed.
    ///
    /// Two layers sit over the code default: the user's file, named by the
    /// `theme` preference and read from disk, and the project's, named by
    /// `editor_settings.yaml` and loaded through the project's asset store so
    /// the host's watcher reimports an edited file. The project wins where two
    /// name the same item. A layer that cannot be read, loaded or merged warns
    /// and is skipped, one broken file never costing the others' values, and a
    /// resolution that repeats the theme already applied leaves the generation
    /// alone. The poll re-merges at least once per [`STATE_FILE_POLL_FRAMES`]
    /// frames and whenever a stamp moves.
    fn watch_theme(&mut self) {
        let due = self.frame_index.is_multiple_of(STATE_FILE_POLL_FRAMES);
        let state_directory = layout_directory();
        // Step 1: the user layer. The preferences reader ran before this, so
        // the document is this frame's.
        let value = preferences::lookup(
            &self.preferences_document,
            preferences::THEME_PREFERENCE_KEY,
        )
        .and_then(|value| value.as_str().map(str::to_string));
        let selection = UserThemeSelection::from_preference(value.as_deref(), &state_directory);
        let user_stamp = match &selection {
            UserThemeSelection::File(path) => FileStamp::of(path),
            _ => FileStamp::default(),
        };
        let user_moved =
            selection != self.theme_watch.selection || user_stamp != self.theme_watch.user_stamp;
        self.theme_watch.selection = selection.clone();
        self.theme_watch.user_stamp = user_stamp;
        // A name an earlier build understood names no file; saying so once per
        // value is all there is to do about it.
        if let UserThemeSelection::Legacy(name) = &selection {
            if self.theme_watch.legacy_logged.as_deref() != Some(name.as_str()) {
                self.theme_watch.legacy_logged = Some(name.clone());
                warn!(
                    target: telemetry_target::ENGINE,
                    "[editor] theme name '{name}' is not a file; no user layer"
                );
            }
        }
        // Step 2: the project layer's settings file. The project root is the
        // parent of the asset directory the host resolved.
        let project_directory = self
            .host
            .host()
            .asset_directory()
            .and_then(std::path::Path::parent)
            .map(std::path::Path::to_path_buf);
        let settings_stamp = project_directory
            .as_ref()
            .map(|directory| FileStamp::of(&directory.join(pill_host::EDITOR_SETTINGS_FILE)))
            .unwrap_or_default();
        if settings_stamp != self.theme_watch.settings_stamp || self.theme_watch.settings.is_none()
        {
            self.theme_watch.settings_stamp = settings_stamp;
            self.theme_watch.settings = Some(match &project_directory {
                Some(directory) => pill_host::read_editor_settings(directory),
                None => Ok(pill_host::EditorSettings::default()),
            });
            if let Some(Err(message)) = &self.theme_watch.settings {
                warn!(
                    target: telemetry_target::ENGINE,
                    "[editor] theme refused layer=project: {message}"
                );
            }
        }
        let project_path = match (&project_directory, &self.theme_watch.settings) {
            (Some(directory), Some(Ok(settings))) => {
                pill_host::resolve_editor_theme(directory, settings)
            }
            _ => None,
        };
        let path_moved = project_path != self.theme_watch.project_path;
        self.theme_watch.project_path = project_path;
        // The file itself, so an edit reaches the editor even when the host's
        // watcher has not reimported it yet: one stat per poll.
        let project_stamp = self
            .host
            .host()
            .asset_directory()
            .zip(self.theme_watch.project_path.as_ref())
            .map(|(root, path)| FileStamp::of(&root.join(path)))
            .unwrap_or_default();
        let file_moved = project_stamp != self.theme_watch.project_stamp;
        self.theme_watch.project_stamp = project_stamp;
        // The font files the applied theme names move the
        // same way: a stamp per family, and an edit re-installs the fonts
        // without waiting for a theme change.
        let fonts_moved = self.stamp_fonts();
        if !user_moved
            && !path_moved
            && !file_moved
            && !fonts_moved
            && !due
            && self.project_layer_settled()
        {
            return;
        }
        // Step 3: the project layer's document, loaded when the path changed
        // and re-read when its file did.
        let project_overrides = self.load_project_layer(path_moved, file_moved);
        // Step 4: the user layer's document. A file that does not parse is a
        // refused layer, not an empty one.
        let user_overrides = match &selection {
            UserThemeSelection::File(path) => match std::fs::read_to_string(path) {
                Ok(text) => match ThemeOverrides::from_json_str(&text) {
                    Ok(overrides) => Some(overrides),
                    Err(error) => {
                        warn!(
                            target: telemetry_target::ENGINE,
                            "[editor] theme refused layer=user: {error}"
                        );
                        None
                    }
                },
                Err(error) => {
                    warn!(
                        target: telemetry_target::ENGINE,
                        "[editor] theme refused layer=user: `{}` could not be read: {error}",
                        path.display()
                    );
                    None
                }
            },
            _ => None,
        };
        // Step 5: merge in order, and publish a new generation when the result
        // is not the theme already painted.
        let user_info = match &selection {
            UserThemeSelection::File(path) => {
                Some(ThemeLayerInfo::user(path.display().to_string()))
            }
            _ => None,
        };
        let project_info = self
            .theme_watch
            .project_path
            .clone()
            .map(ThemeLayerInfo::project);
        let user_pair = user_info.as_ref().zip(user_overrides.as_ref());
        let project_pair = project_info.as_ref().zip(project_overrides.as_ref());
        let merge = merge_layers(
            &pill_editor_core::style::default_theme(),
            user_pair,
            project_pair,
        );
        for (layer, message) in &merge.refused {
            warn!(
                target: telemetry_target::ENGINE,
                "[editor] theme refused layer={layer}: {message}"
            );
        }
        // Step 6: the fonts the applied layers name, each resolved against the
        // layer that named it.
        let user_directory = match &selection {
            UserThemeSelection::File(path) => path.parent().map(std::path::Path::to_path_buf),
            _ => None,
        };
        self.refresh_fonts(
            &merge.theme,
            user_overrides.as_ref(),
            project_overrides.as_ref(),
            user_directory.as_deref(),
        );
        if merge.theme == self.theme
            && merge.layers == self.theme_layers
            && self.theme_watch.fonts == self.theme_fonts
        {
            return;
        }
        self.theme = merge.theme;
        self.theme_layers = merge.layers;
        self.theme_fonts = self.theme_watch.fonts.clone();
        self.theme_generation = self.theme_generation.wrapping_add(1);
        info!(
            target: telemetry_target::ENGINE,
            layers = %layers_text(&self.theme_layers),
            generation = self.theme_generation,
            "[editor] theme resolved"
        );
    }

    /// Stamp each family's resolved font source and report whether one moved.
    ///
    /// One `stat` per family per poll; the bytes are only read when a stamp
    /// moved, which is what keeps a 30-frame poll from re-reading fonts.
    fn stamp_fonts(&mut self) -> bool {
        let mut moved = false;
        for (index, source) in self.theme_watch.font_sources.iter().enumerate() {
            let stamp = match source {
                Some(FontSource::Project(path)) => self
                    .host
                    .host()
                    .asset_directory()
                    .map(|root| FileStamp::of(&root.join(path)))
                    .unwrap_or_default(),
                Some(FontSource::User(path)) => FileStamp::of(path),
                None => FileStamp::default(),
            };
            if stamp != self.theme_watch.font_stamps[index] {
                moved = true;
            }
            self.theme_watch.font_stamps[index] = stamp;
        }
        moved
    }

    /// Resolve the fonts the applied layers name into the bytes the kit
    /// installs.
    ///
    /// The merged theme's paths say which family each layer named, so a layer
    /// that names only `body` keeps the lower layer's other two. A project font
    /// loads through the asset store (it must be under `editor/`); a user font
    /// is a file relative to the user document's own directory. A font that
    /// cannot be loaded warns and leaves its family to the built-in ones, with
    /// the family name still registered.
    fn refresh_fonts(
        &mut self,
        theme: &pill_ui_theme::Theme,
        user: Option<&ThemeOverrides>,
        project: Option<&ThemeOverrides>,
        user_directory: Option<&std::path::Path>,
    ) {
        let families: [(&Option<String>, Option<&String>, Option<&String>); 3] = [
            (
                &theme.fonts.body,
                user.and_then(|overrides| overrides.fonts.body.as_ref()),
                project.and_then(|overrides| overrides.fonts.body.as_ref()),
            ),
            (
                &theme.fonts.display,
                user.and_then(|overrides| overrides.fonts.display.as_ref()),
                project.and_then(|overrides| overrides.fonts.display.as_ref()),
            ),
            (
                &theme.fonts.monospace,
                user.and_then(|overrides| overrides.fonts.monospace.as_ref()),
                project.and_then(|overrides| overrides.fonts.monospace.as_ref()),
            ),
        ];
        let mut fonts = pill_editor_core::kit::style::ThemeFonts::default();
        let mut sources: [Option<FontSource>; 3] = [None, None, None];
        for (index, (merged, user_path, project_path)) in families.into_iter().enumerate() {
            let Some(path) = merged.as_deref() else {
                continue;
            };
            // The layer that named this family wins; the merged path is one of
            // the two layer paths by construction.
            let source = if project_path.map(String::as_str) == Some(path) {
                FontSource::Project(path.to_string())
            } else if user_path.map(String::as_str) == Some(path) {
                let file = match user_directory {
                    Some(directory) => directory.join(path),
                    None => std::path::PathBuf::from(path),
                };
                FontSource::User(file)
            } else {
                // Named by the code default, which the editor does not do;
                // there is no layer to resolve it against.
                continue;
            };
            let bytes = self.read_font(&source);
            sources[index] = Some(source);
            match index {
                0 => fonts.body = bytes,
                1 => fonts.display = bytes,
                _ => fonts.monospace = bytes,
            }
        }
        self.theme_watch.font_sources = sources;
        self.theme_watch.fonts = fonts;
    }

    /// The bytes one font source holds, or `None` with the refusal logged.
    fn read_font(&mut self, source: &FontSource) -> Option<Vec<u8>> {
        match source {
            FontSource::Project(path) => {
                let world = self.host.host_mut().engine_mut().world_mut();
                match pill_ui_theme::load_font(world, path, ThemePathScope::Editor) {
                    Ok(loaded) => world
                        .get_resource::<pill_engine_core::AssetManager>()
                        .and_then(|assets| assets.get(loaded.handle))
                        .map(|font| font.bytes.clone()),
                    Err(message) => {
                        warn!(
                            target: telemetry_target::ENGINE,
                            "[editor] font refused: {message}"
                        );
                        None
                    }
                }
            }
            FontSource::User(file) => match std::fs::read(file) {
                Ok(bytes) if !bytes.is_empty() => Some(bytes),
                Ok(_) => {
                    warn!(
                        target: telemetry_target::ENGINE,
                        "[editor] font refused: `{}` is empty",
                        file.display()
                    );
                    None
                }
                Err(error) => {
                    warn!(
                        target: telemetry_target::ENGINE,
                        "[editor] font refused: `{}` could not be read: {error}",
                        file.display()
                    );
                    None
                }
            },
        }
    }

    /// Whether the project layer's content is the version last merged, so a
    /// poll that found no stamp moved can stop before merging.
    fn project_layer_settled(&mut self) -> bool {
        let Some(handle) = self.theme_watch.project_handle else {
            return true;
        };
        let world = self.host.host_mut().engine_mut().world();
        let version = world
            .get_resource::<pill_engine_core::AssetManager>()
            .and_then(|assets| assets.content_version(handle));
        version.is_none() || version == self.theme_watch.project_version
    }

    /// Load the project layer's document, reusing the cached handle.
    ///
    /// `path_moved` means the resolved path is not the one the handle was
    /// loaded from, so the file is imported (and scope-checked) again;
    /// `file_moved` means the same file changed, so the store's copy is
    /// re-read.
    fn load_project_layer(&mut self, path_moved: bool, file_moved: bool) -> Option<ThemeOverrides> {
        let Some(path) = self.theme_watch.project_path.clone() else {
            self.theme_watch.project_handle = None;
            self.theme_watch.project_version = None;
            return None;
        };
        if path_moved || self.theme_watch.project_handle.is_none() {
            self.theme_watch.project_handle = None;
            self.theme_watch.project_version = None;
            let world = self.host.host_mut().engine_mut().world_mut();
            match load_overrides(world, &path, ThemePathScope::Editor) {
                Ok(handle) => self.theme_watch.project_handle = Some(handle),
                Err(message) => {
                    warn!(
                        target: telemetry_target::ENGINE,
                        "[editor] theme refused layer=project: {message}"
                    );
                    return None;
                }
            }
        } else if file_moved {
            let world = self.host.host_mut().engine_mut().world_mut();
            let assets = world.get_resource_mut::<pill_engine_core::AssetManager>()?;
            if let Err(error) =
                assets.reimport_standalone::<ThemeOverrides>(std::path::Path::new(&path))
            {
                warn!(
                    target: telemetry_target::ENGINE,
                    "[editor] theme refused layer=project: `{path}` did not load: {error}"
                );
                return None;
            }
        }
        let handle = self.theme_watch.project_handle?;
        let world = self.host.host_mut().engine_mut().world();
        let assets = world.get_resource::<pill_engine_core::AssetManager>()?;
        self.theme_watch.project_version = assets.content_version(handle);
        assets.get(handle).cloned()
    }

    /// Applies the keybindings file whenever it changes.
    ///
    /// The registry owns the preset and the overrides; this only reads the
    /// file, hands it over and reports what came out of it - the preset, the
    /// number of overrides, every problem the file's text had, and one line
    /// per chord that two commands now answer to. A missing or malformed file
    /// is an empty one: the built-ins' own bindings stand.
    fn watch_keybindings(&mut self) {
        let directory = layout_directory();
        let path = directory.join(KEYBINDINGS_FILE);
        let stamp = FileStamp::of(&path);
        let due = self.frame_index.is_multiple_of(STATE_FILE_POLL_FRAMES);
        if !due && self.keybindings_applied && stamp == self.keybindings_stamp {
            return;
        }
        self.keybindings_stamp = stamp;
        let text = std::fs::read_to_string(&path).ok();
        if self.keybindings_applied && text == self.keybindings_text {
            return;
        }
        self.keybindings_applied = true;
        self.keybindings_text = text;
        let file = self
            .keybindings_text
            .as_deref()
            .and_then(|text| serde_json::from_str::<KeybindingsFile>(text).ok())
            .unwrap_or_else(editor_default_keybindings);
        let world = self.host.host_mut().engine_mut().world_mut();
        CommandRegistry::ensure(world);
        let (problems, preset, overrides, conflicts) = {
            let Some(registry) = world.get_resource_mut::<CommandRegistry>() else {
                return;
            };
            let problems = registry.apply_file(&file);
            let preset = registry.preset();
            let overrides = registry.override_count();
            let conflicts: Vec<(String, String)> = registry
                .conflicts()
                .into_iter()
                .map(|(chord, ids)| (chord.to_string(), ids.join(",")))
                .collect();
            (problems, preset, overrides, conflicts)
        };
        info!(
            target: telemetry_target::ENGINE,
            preset = %preset,
            overrides,
            "[editor] keybindings applied"
        );
        // Each problem and each conflicting chord is also a toast: a
        // rebinding that silently stops working is exactly what the overlay
        // is for.
        for problem in problems {
            warn!(target: telemetry_target::ENGINE, "{}", problem.as_str());
            self.push_toast(NoticeLevel::Warning, problem);
        }
        for (chord, commands) in conflicts {
            warn!(
                target: telemetry_target::ENGINE,
                chord = chord.as_str(),
                commands = commands.as_str(),
                "[editor] keybinding conflict"
            );
            self.push_toast(
                NoticeLevel::Warning,
                format!("Keybinding conflict: {chord} is bound to {commands}"),
            );
        }
    }

    /// Open the Problems row a click asked for.
    ///
    /// The `code_editor_command` preference names the program and its flags;
    /// the row's `file:line` goes last as one argument, which is the form VS
    /// Code, CLion and every other editor's command line accepts. A missing
    /// or empty preference falls back to `code`.
    fn drain_problem_open(&mut self) {
        let request = {
            let world = self.host.host_mut().engine_mut().world_mut();
            world
                .get_resource_mut::<EguiShellState>()
                .and_then(|state| state.problem_open.take())
        };
        let Some(index) = request else {
            return;
        };
        info!(
            target: telemetry_target::ENGINE,
            index,
            problems = self.notices.problems.len(),
            "[editor] problem open requested"
        );
        let Some(problem) = self.notices.problems.get(index).cloned() else {
            return;
        };
        let Some(file) = problem.problem.file.clone() else {
            return;
        };
        let target = match problem.problem.line {
            Some(line) => format!("{file}:{line}"),
            None => file,
        };
        let command = self
            .preferences_text
            .as_deref()
            .and_then(|text| serde_json::from_str::<EditorPreferences>(text).ok())
            .and_then(|preferences| preferences.code_editor_command)
            .filter(|command| !command.trim().is_empty())
            .unwrap_or_else(|| "code".to_string());
        let mut parts = split_command(&command).into_iter();
        let Some(program) = parts.next() else {
            return;
        };
        match std::process::Command::new(&program)
            .args(parts)
            .arg(&target)
            .spawn()
        {
            Ok(_child) => info!(
                target: telemetry_target::ENGINE,
                command = command.as_str(),
                target = target.as_str(),
                "[editor] problem opened"
            ),
            Err(error) => warn!(
                target: telemetry_target::ENGINE,
                command = command.as_str(),
                error = %error,
                "[editor] problem open failed"
            ),
        }
    }

    /// Run `problem open <index>`: the same request a click on the row makes.
    ///
    /// The request is drained at once rather than on the next frame: a suite
    /// that sends this line waits for the editor it launches, and one frame of
    /// delay would make the assertion race the spawn.
    fn scripted_problem_open(&mut self, index: usize) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let found = world
            .get_resource_mut::<EguiShellState>()
            .map(|state| {
                state.problem_open = Some(index);
            })
            .is_some();
        info!(
            target: telemetry_target::ENGINE,
            index,
            found,
            "[editor] problem open scripted"
        );
        self.drain_problem_open();
    }

    /// Log the Statistics tab's numbers, for the suites.
    fn dump_stats(&mut self) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(state) = world.get_resource::<EguiShellState>() else {
            return;
        };
        let stats = &state.stats;
        info!(
            target: telemetry_target::ENGINE,
            fps = stats.frames_per_second,
            shell_us = stats.shell_micros,
            passes = stats.shell_passes,
            tabs = stats.tabs.len(),
            "[test] stats"
        );
        for entry in &stats.tabs {
            info!(
                target: telemetry_target::ENGINE,
                name = entry.tab.name(),
                us = entry.micros,
                passes = entry.passes,
                "[test] stats tab"
            );
        }
    }

    /// Log the redraw counters, for the suites.
    ///
    /// Two dumps separated by an idle stretch tell the posture apart: with the
    /// loop waiting, the frame count grows by the idle poll's pace, not by the
    /// display's.
    fn dump_cadence(&mut self) {
        let paused = self.host.host().engine().systems_paused();
        info!(
            target: telemetry_target::ENGINE,
            continuous = self.cadence.continuous(),
            frames = self.cadence.frames(),
            demanded = self.cadence.demanded(),
            waits = self.cadence.waits(),
            paused,
            input = self.last_demand.input,
            scripted = self.last_demand.scripted,
            snapshot = self.last_demand.snapshot,
            resized = self.last_demand.resized,
            repaint = self.last_demand.repaint_requested,
            animating = self.last_demand.animating,
            "[test] cadence"
        );
    }

    /// `panel window <slot> <id>`: ask a panel to open one of its windows.
    ///
    /// The panel decides what the id means, so this drives the same code its
    /// button does.
    fn scripted_panel_window(&mut self, slot: u16, id: &str) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(state) = world.get_resource_mut::<EguiShellState>() else {
            warn!(
                target: telemetry_target::ENGINE,
                "[editor] panel window request needs the shell module's state"
            );
            return;
        };
        state.panel_window_ping = Some((slot, id.to_string()));
        info!(
            target: telemetry_target::ENGINE,
            slot,
            window = id,
            "[editor] panel window requested"
        );
    }

    /// `select resource <name>`: the recorded selection a resource row makes.
    fn scripted_select_resource(&mut self, name: &str) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() else {
            return;
        };
        pending.push_recorded(
            vec![EditorCommand::SelectResource {
                resource: Some(name.to_string()),
            }],
            format!("select resource {name}"),
            None,
        );
        info!(
            target: telemetry_target::ENGINE,
            resource = name,
            "[editor] resource selection queued"
        );
    }

    /// `edit resource <field> <value>`: one recorded `SetResourceField`.
    fn scripted_edit_resource(&mut self, resource: &str, field: &str, value_text: &str) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let current = world.get_resource::<GizmoState>().and_then(|state| {
            pill_editor_core::gizmo::resource_field_value(&state.settings, resource, field).ok()
        });
        let Some(current) = current else {
            warn!(
                target: telemetry_target::ENGINE,
                resource,
                field,
                "[editor] resource edit: not an editable field"
            );
            return;
        };
        let value = match inspector::parse_field_value(value_text, &current) {
            Ok(value) => value,
            Err(message) => {
                warn!(
                    target: telemetry_target::ENGINE,
                    field,
                    message = message.as_str(),
                    "[editor] resource edit: value not understood"
                );
                return;
            }
        };
        let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() else {
            return;
        };
        pending.push_recorded(
            vec![EditorCommand::SetResourceField {
                resource: resource.to_string(),
                field: field.to_string(),
                value,
            }],
            format!("set {resource}.{field}"),
            Some(format!("resource.{resource}.{field}")),
        );
        info!(
            target: telemetry_target::ENGINE,
            resource,
            field,
            value = value_text,
            "[editor] resource edit queued"
        );
    }

    /// Log the entity list and each row's component names, for the suites: a
    /// collection test finds its subject this way.
    fn dump_entities(&mut self) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let entities = world
            .get_resource::<EditorSnapshot>()
            .map(|snapshot| snapshot.entities.clone())
            .unwrap_or_default();
        info!(
            target: telemetry_target::ENGINE,
            count = entities.len(),
            "[test] entities"
        );
        for entry in &entities {
            info!(
                target: telemetry_target::ENGINE,
                display = entry.display.as_str(),
                components = entry.archetype_label.as_str(),
                "[test] entity"
            );
        }
    }

    /// Log the editable resources and their values, for the suites.
    fn dump_resources(&mut self) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let resources = world
            .get_resource::<EditorSnapshot>()
            .map(|snapshot| snapshot.resources.clone())
            .unwrap_or_default();
        info!(
            target: telemetry_target::ENGINE,
            count = resources.len(),
            "[test] resources"
        );
        for view in &resources {
            info!(
                target: telemetry_target::ENGINE,
                name = view.name.as_str(),
                label = view.label.as_str(),
                fields = view.fields.len(),
                "[test] resource"
            );
            for field in &view.fields {
                info!(
                    target: telemetry_target::ENGINE,
                    resource = view.name.as_str(),
                    field = field.field.as_str(),
                    label = field.label.as_str(),
                    value = inspector::value_text(&field.value).as_str(),
                    "[test] resource field"
                );
            }
        }
    }

    /// Log the panel windows on screen, for the suites.
    fn dump_windows(&mut self) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let windows = world
            .get_resource::<EguiShellState>()
            .map(|state| state.panel_windows.windows.clone())
            .unwrap_or_default();
        info!(
            target: telemetry_target::ENGINE,
            count = windows.len(),
            "[test] windows"
        );
        for window in &windows {
            info!(
                target: telemetry_target::ENGINE,
                id = window.id.as_str(),
                source = window.source.as_str(),
                slot = window.slot,
                modal = window.modal,
                "[test] window"
            );
        }
    }

    /// Log the module panels, for the suites.
    fn dump_panels(&mut self) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(registry) = world.get_resource::<PanelRegistry>() else {
            return;
        };
        let budget = world
            .get_resource::<EguiShellState>()
            .map(|state| state.panel_budget.clone())
            .unwrap_or_default();
        info!(
            target: telemetry_target::ENGINE,
            count = registry.len(),
            live = registry
                .entries()
                .iter()
                .filter(|entry| entry.alive())
                .count(),
            "[test] panels"
        );
        for (slot, entry) in registry.entries().iter().enumerate() {
            let timing = budget.get(slot as u16);
            info!(
                target: telemetry_target::ENGINE,
                slot,
                id = entry.id.as_str(),
                source = entry.source.as_str(),
                alive = entry.alive(),
                epoch = entry.epoch,
                micros = timing.map(|timing| timing.micros).unwrap_or(0.0),
                passes = timing.map(|timing| timing.passes).unwrap_or(0),
                slow = budget.is_slow(slot as u16),
                title = registry.tab_title(slot as u16).as_str(),
                "[test] panel"
            );
        }
    }

    /// `panel unload <source>`: drop one module's liveness marker.
    ///
    /// This is the state a module's unload leaves the registry in - the marker
    /// is what the unload would drop - so the tab reads "module not loaded"
    /// and the panel's draw function is no longer handed out.
    fn scripted_panel_unload(&mut self, source: &str) {
        let dropped = self.panel_markers.remove(source).is_some();
        info!(
            target: telemetry_target::ENGINE,
            source,
            dropped,
            "[editor] panel source unloaded"
        );
    }

    /// `panel restore <source>`: bind that module's panels again.
    ///
    /// The requests the executable kept are registered against a fresh marker,
    /// which is what a reload's re-registration does.
    fn scripted_panel_restore(&mut self, source: &str) {
        if self.panel_markers.contains_key(source) {
            info!(
                target: telemetry_target::ENGINE,
                source,
                restored = 0,
                "[editor] panel source restored"
            );
            return;
        }
        let marker = ModuleLiveness::new();
        let kept: Vec<PanelRequest> = self
            .panel_requests
            .iter()
            .copied()
            .filter(|request| request.source == source)
            .collect();
        let world = self.host.host_mut().engine_mut().world_mut();
        let mut restored = 0;
        if let Some(registry) = world.get_resource_mut::<PanelRegistry>() {
            for request in &kept {
                registry.register(
                    request.id,
                    request.title,
                    request.source,
                    request.draw,
                    &marker,
                );
                restored += 1;
            }
        }
        if restored > 0 {
            self.panel_markers.insert(source.to_string(), marker);
        }
        info!(
            target: telemetry_target::ENGINE,
            source,
            restored,
            "[editor] panel source restored"
        );
    }

    /// Log the Problems rows, for the suites.
    fn dump_problems(&mut self) {
        info!(
            target: telemetry_target::ENGINE,
            count = self.notices.problems.len(),
            "[test] problems"
        );
        for row in self.notices.problems.iter().take(10) {
            info!(
                target: telemetry_target::ENGINE,
                module = row.module.as_str(),
                severity = row.problem.severity.name(),
                file = row.problem.file.as_deref().unwrap_or(""),
                line = row.problem.line.unwrap_or(0),
                text = row.problem.message.as_str(),
                "[test] problem row"
            );
        }
    }

    /// Fold the capture ring's new lines into the Console's history.
    ///
    /// One owned copy per line, capped by the model: the ring's own strings
    /// belong to `pill_core`, and the tab must outlive every reload that
    /// happened while the line was being written.
    fn publish_console_lines(&mut self) {
        // The handle is cloned out first: the world borrow below holds `self`
        // mutably, and the ring is a shared handle rather than the window's.
        let buffer = self.log_buffer.clone();
        let world = self.host.host_mut().engine_mut().world_mut();
        // The tab's filters live in the shell session; applying them here means
        // this frame's snapshot already shows what the person just chose.
        let filters = world
            .get_resource::<EguiShellState>()
            .map(|state| state.console_filters.clone());
        if let (Some(filters), Some(state)) = (filters, world.get_resource_mut::<ConsoleState>()) {
            if state.settings() != filters {
                state.apply(&filters);
            }
        }
        let Some(state) = world.get_resource_mut::<ConsoleState>() else {
            return;
        };
        let after = state.last_seq;
        for line in buffer.since(after) {
            let channel = ConsoleChannel::of_target(&line.target);
            state.push(ConsoleLine {
                seq: line.seq,
                level: line.level,
                target: line.target,
                message: line.message,
                channel,
            });
        }
    }

    /// Run `console filter <level>`: the lowest level the Console tab shows,
    /// which the next frame applies to the history.
    fn scripted_console_filter(&mut self, level: &str) {
        let Some(value) = pill_editor_core::console::level_from_name(level) else {
            warn!(
                target: telemetry_target::ENGINE,
                level,
                "[editor] console filter ignored"
            );
            return;
        };
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(state) = world.get_resource_mut::<EguiShellState>() {
            state.console_filters.minimum_level = value;
        }
        info!(
            target: telemetry_target::ENGINE,
            level,
            "[editor] console filter"
        );
    }

    /// Run `console search <text>`: the Console tab's substring filter.
    fn scripted_console_search(&mut self, text: String) {
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(state) = world.get_resource_mut::<EguiShellState>() {
            state.console_filters.search = text.clone();
        }
        info!(
            target: telemetry_target::ENGINE,
            search = text.as_str(),
            "[editor] console search"
        );
    }

    /// Log the Console tab's filters and the rows they leave, for the suites.
    fn dump_console(&mut self) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(state) = world.get_resource::<ConsoleState>() else {
            return;
        };
        let view = state.view();
        // The per-channel counts are what proves a project's own line reached
        // the history: the game channel only fills when something outside the
        // engine logs.
        let engine = state
            .lines
            .iter()
            .filter(|line| line.channel == ConsoleChannel::Engine)
            .count();
        let game = state.lines.len() - engine;
        let first = view
            .lines
            .first()
            .map(|line| format!("{} {}: {}", line.level_name(), line.target, line.message))
            .unwrap_or_default();
        info!(
            target: telemetry_target::ENGINE,
            level = pill_editor_core::console::level_name(view.minimum_level),
            channel = view.channel.map_or("both", ConsoleChannel::name),
            search = view.search.as_str(),
            total = view.total,
            visible = view.visible,
            engine,
            game,
            first = first.as_str(),
            "[test] console"
        );
    }

    /// Run `palette <query>` / `palette run` / `palette close` from the
    /// scripted channel.
    ///
    /// This touches the palette's model state only; the shell's pass draws the
    /// popup and runs the selected row, so a scripted run and a click take the
    /// same action path.
    fn scripted_palette(&mut self, request: PaletteRequest) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(state) = world.get_resource_mut::<EguiShellState>() else {
            return;
        };
        match request {
            PaletteRequest::Query(query) => {
                state.palette.open = true;
                state.palette.query = query;
                state.palette.selected = 0;
                state.palette.logged_query = None;
                info!(
                    target: telemetry_target::ENGINE,
                    query = state.palette.query.as_str(),
                    "[editor] palette opened"
                );
            }
            PaletteRequest::Run => state.palette.run_requested = true,
            PaletteRequest::Close => {
                state.palette.open = false;
                state.palette.run_requested = false;
                info!(
                    target: telemetry_target::ENGINE,
                    "[editor] palette dismissed"
                );
            }
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
    /// `SetGizmoTool` edit the toolbox rows and the viewport menu queue
    /// (applied, never recorded - a tool choice is a control, not an edit).
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

    /// Report the open scene and its unsaved marker when they change.
    ///
    /// The title is recomputed every frame from the session resource and
    /// logged only when it changed, so the log names each state the document
    /// went through - which is what the editor suite waits for. The window's
    /// own title names the project, so nothing here touches it.
    fn report_scene_title(&mut self) {
        let title = {
            let world = self.host.host_mut().engine_mut().world_mut();
            world
                .get_resource::<pill_editor_core::scene_workflow::SceneSession>()
                .map(pill_editor_core::scene_workflow::SceneSession::title)
                .unwrap_or_else(|| "untitled".to_string())
        };
        if title == self.scene_title {
            return;
        }
        info!(
            target: telemetry_target::ENGINE,
            title = title.as_str(),
            "[editor] scene title"
        );
        self.scene_title = title;
    }

    /// `scene new|open|save` from the scripted channel: the same command the
    /// File menu queues, applied at the next frame boundary.
    ///
    /// Document commands are not recorded - they replace the world rather than
    /// edit it - so the batch is queued unrecorded.
    fn scripted_scene(&mut self, command: EditorCommand) {
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push(vec![command]);
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
                .map(|selection| (selection.primary(), selection.asset.clone()))
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

    /// Hand one asset to the OS as a file drag.
    ///
    /// The shell reports the asset's project-relative path; the target needs a
    /// file on disk, so it is resolved through the host's asset directory
    /// first. The drag itself blocks in OLE's modal loop, which is why nothing
    /// else happens on this thread until the person drops or cancels.
    fn start_drag_out(&mut self, path: &str) {
        let Some(root) = self
            .host
            .host_mut()
            .asset_directory()
            .map(|directory| directory.to_path_buf())
        else {
            warn!(
                target: telemetry_target::ENGINE,
                path,
                "[editor] drag out refused: no asset directory"
            );
            return;
        };
        // The listing's paths are relative to `res`; the file on disk is not.
        // An absolute path is already what the OS needs (the scripted channel
        // names one), so only a relative path is resolved through the root.
        let relative = path.strip_prefix("res/").unwrap_or(path);
        let absolute = if std::path::Path::new(relative).is_absolute() {
            std::path::PathBuf::from(relative)
        } else {
            root.join(relative)
        };
        match crate::drag_out::start(&absolute.to_string_lossy()) {
            crate::drag_out::DragOutOutcome::Dropped(effect) => info!(
                target: telemetry_target::ENGINE,
                path = %absolute.display(),
                effect = effect.0,
                "[editor] drag out finished"
            ),
            crate::drag_out::DragOutOutcome::Cancelled => info!(
                target: telemetry_target::ENGINE,
                path = %absolute.display(),
                "[editor] drag out cancelled"
            ),
            // A drag needs the pointer's button down; without it the modal
            // loop would never see the input that ends it.
            crate::drag_out::DragOutOutcome::NoButtonHeld => info!(
                target: telemetry_target::ENGINE,
                path = %absolute.display(),
                "[editor] drag out skipped: no button held"
            ),
            crate::drag_out::DragOutOutcome::Failed => {}
        }
        // A drag is a frame's worth of interaction of its own: the frame after
        // it owes a redraw, so the editor visibly settles.
        self.demand.input = true;
    }

    /// Dock one more scene view, the Window menu's own request: the tab goes
    /// in exactly where the menu would put it, and the executable's activation
    /// path brings it to the front.
    fn scripted_new_scene_view(&mut self) {
        let next = {
            let world = self.host.host_mut().engine_mut().world_mut();
            world.get_resource::<EguiShellState>().map(|state| {
                (
                    state.dock.next_scene_view(),
                    state.dock.can_open_scene_view(),
                )
            })
        };
        let Some((tab, fits)) = next else {
            warn!(
                target: telemetry_target::ENGINE,
                "[editor] scene view needs the shell module's state"
            );
            return;
        };
        if !fits {
            warn!(
                target: telemetry_target::ENGINE,
                limit = pill_editor_core::boundary::SCENE_VIEWS_MAX,
                "[editor] scene view refused: the view limit is reached"
            );
            return;
        }
        self.activate_tab(tab);
        info!(
            target: telemetry_target::ENGINE,
            tab = tab.name().as_str(),
            view = tab.scene_view().unwrap_or_default(),
            "[editor] scene view added"
        );
    }

    /// Apply the shell's IME state to the main window.
    ///
    /// egui decides when a text field wants the input method and where its
    /// caret is; this executable is what owns the window, so it is the only
    /// place that can pass those decisions to the platform. The rect is in
    /// logical points, which is the window's IME coordinate space.
    fn apply_ime(&mut self, allowed: bool, rect: Option<pill_editor_core::boundary::ShellRect>) {
        let window = self.window.clone();
        if allowed != self.ime_allowed {
            window.set_ime_allowed(allowed);
            self.ime_allowed = allowed;
            info!(
                target: telemetry_target::ENGINE,
                allowed,
                "[editor] ime allowed changed"
            );
        }
        if !allowed {
            return;
        }
        if let Some(rect) = rect {
            if self.ime_rect != Some((rect.x, rect.y, rect.width, rect.height)) {
                self.ime_rect = Some((rect.x, rect.y, rect.width, rect.height));
                window.set_ime_cursor_area(
                    winit::dpi::LogicalPosition::new(rect.x as f64, rect.y as f64),
                    winit::dpi::LogicalSize::new(rect.width as f64, rect.height as f64),
                );
                info!(
                    target: telemetry_target::ENGINE,
                    x = rect.x,
                    y = rect.y,
                    width = rect.width,
                    height = rect.height,
                    "[editor] ime cursor area"
                );
            }
        }
    }

    /// Reconcile the popped-out windows with the shell's frames and paint
    /// them.
    ///
    /// The shell reports one frame per popped-out tab, so the frames *are* the
    /// window set: a tab that appears gets a window, a tab that disappears has
    /// its window closed, and each remaining frame is painted into its own
    /// swapchain through its own renderer. The pass ran in the shell, so
    /// nothing here touches egui's context.
    fn present_popouts(
        &mut self,
        event_loop: &ActiveEventLoop,
        frames: &mut [pill_editor_core::boundary::PopoutFrame],
    ) {
        for frame in frames.iter() {
            if self.popouts.contains_key(&frame.tab) {
                continue;
            }
            self.open_popout(event_loop, &frame.tab, &frame.title);
        }
        // A tab that redocked (or was closed) loses its window.
        let live: Vec<String> = frames.iter().map(|frame| frame.tab.clone()).collect();
        let stale: Vec<String> = self
            .popouts
            .keys()
            .filter(|tab| !live.contains(tab))
            .cloned()
            .collect();
        for tab in stale {
            if self.popouts.remove(&tab).is_some() {
                info!(
                    target: telemetry_target::ENGINE,
                    tab = tab.as_str(),
                    "[editor] popout closed"
                );
            }
        }
        for frame in frames.iter() {
            let Some(popout) = self.popouts.get(&frame.tab) else {
                continue;
            };
            // A popped-out Scene tab reports its viewport in the pop-out's own
            // coordinates; the view's window follows it onto that window's
            // screen position, which is what lets it cross monitors.
            if frame.frame.scene_rects.iter().all(|rect| rect.is_none()) {
                continue;
            }
            let scale_factor = popout.window.scale_factor() as f32;
            let origin = popout
                .window
                .inner_position()
                .unwrap_or(PhysicalPosition::new(0, 0));
            let placements: Vec<(usize, i32, i32, u32, u32)> = frame
                .frame
                .scene_rects
                .iter()
                .enumerate()
                .filter_map(|(view, rect)| {
                    let rect = (*rect)?;
                    Some((
                        view,
                        origin.x + (rect.x * scale_factor) as i32,
                        origin.y + (rect.y * scale_factor) as i32,
                        (rect.width * scale_factor).max(1.0) as u32,
                        (rect.height * scale_factor).max(1.0) as u32,
                    ))
                })
                .collect();
            for (view, x, y, width, height) in placements {
                self.pop_view_window(event_loop, view, x, y, width, height);
            }
        }
        for frame in frames.iter_mut() {
            let Some(popout) = self.popouts.get_mut(&frame.tab) else {
                continue;
            };
            let textures = std::mem::take(&mut frame.frame.textures);
            match popout.surface.render(
                &self.device,
                &self.queue,
                &mut popout.renderer,
                &frame.frame.meshes,
                textures,
                frame.frame.pixels_per_point,
            ) {
                Ok(drew) => {
                    // The suite's observation point that the window is live,
                    // logged once so a broken painter shows up as a missing
                    // line rather than a silently painted-over window.
                    if drew && !popout.presented {
                        popout.presented = true;
                        info!(
                            target: telemetry_target::ENGINE,
                            tab = frame.tab.as_str(),
                            "[editor] popout presented"
                        );
                    }
                }
                Err(error) => warn!(
                    target: telemetry_target::ENGINE,
                    tab = frame.tab.as_str(),
                    error = %error,
                    "[editor] popout frame failed"
                ),
            }
        }
    }

    /// Append one event to `tab`'s input record, the window's own cursor kept
    /// alongside it.
    fn popout_event(&mut self, tab: &str, event: InputEvent) {
        let cursor = match &event {
            InputEvent::CursorMoved { position } => Some(*position),
            InputEvent::CursorLeft => None,
            _ => None,
        };
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(inputs) = world.get_resource_mut::<EguiPopoutInputs>() else {
            return;
        };
        let record = inputs.by_tab.entry(tab.to_string()).or_default();
        match &event {
            InputEvent::CursorMoved { .. } => record.cursor = cursor,
            InputEvent::CursorLeft => record.cursor = None,
            _ => {}
        }
        record.events.push(event);
    }

    /// Close `tab`'s window and redock its tab: a window close is a redock,
    /// so the tab is never lost by closing it.
    fn close_popout(&mut self, tab: &str) {
        if self.popouts.remove(tab).is_none() {
            return;
        }
        // The tab comes back through the dock's own rule, matched by wire name
        // (the shell owns the tab enum; this side only knows the name).
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(state) = world.get_resource_mut::<EguiShellState>() {
            if let Some(editor_tab) = editor_tab_by_name(tab) {
                state.dock.redock(editor_tab);
            }
        }
        info!(
            target: telemetry_target::ENGINE,
            tab,
            "[editor] popout closed"
        );
    }

    /// `popout <tab>` / `popout <tab> redock`: the same dock
    /// move the tab menu makes, driven from the scripted channel.
    fn scripted_popout(&mut self, tab: EditorTab, redock: bool) {
        if tab == EditorTab::Game {
            warn!(
                target: telemetry_target::ENGINE,
                "[editor] popout refused: the game view is the primary surface's window"
            );
            return;
        }
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(state) = world.get_resource_mut::<EguiShellState>() else {
            warn!(
                target: telemetry_target::ENGINE,
                "[editor] popout needs the shell module's state"
            );
            return;
        };
        let moved = if redock {
            state.dock.redock(tab)
        } else {
            state.dock.pop_out(tab)
        };
        if moved {
            info!(
                target: telemetry_target::ENGINE,
                tab = tab.name().as_str(),
                redock,
                "[editor] popout moved"
            );
        } else {
            warn!(
                target: telemetry_target::ENGINE,
                tab = tab.name().as_str(),
                redock,
                "[editor] popout unchanged: the tab cannot move"
            );
        }
    }

    /// Clear every popped-out window's event list once the frame that read it
    /// has been presented, so no event is replayed into a second pass.
    fn clear_popout_events(&mut self) {
        if self.popouts.is_empty() {
            return;
        }
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(inputs) = world.get_resource_mut::<EguiPopoutInputs>() {
            for record in inputs.by_tab.values_mut() {
                record.events.clear();
            }
        }
    }

    /// Open one popped-out tab's window: a plain top-level window with its own
    /// swapchain and its own egui renderer.
    fn open_popout(&mut self, event_loop: &ActiveEventLoop, tab: &str, title: &str) {
        let attributes = Window::default_attributes()
            .with_title(title)
            .with_inner_size(POPOUT_INITIAL_SIZE);
        let window = match event_loop.create_window(attributes) {
            Ok(window) => window,
            Err(error) => {
                warn!(
                    target: telemetry_target::ENGINE,
                    tab,
                    error = %error,
                    "[editor] popout window refused"
                );
                return;
            }
        };
        let window = Arc::new(window);
        let size = window.inner_size();
        let surface = match OverlaySurface::new_window(
            &self.instance,
            &self.adapter,
            &self.device,
            Arc::clone(&window),
            EDITOR_FORMAT,
            (size.width.max(1), size.height.max(1)),
        ) {
            Ok(surface) => surface,
            Err(error) => {
                warn!(
                    target: telemetry_target::ENGINE,
                    tab,
                    error = %error,
                    "[editor] popout surface refused"
                );
                return;
            }
        };
        let renderer = egui_wgpu::Renderer::new(
            &self.device,
            EDITOR_FORMAT,
            egui_wgpu::RendererOptions::default(),
        );
        info!(
            target: telemetry_target::ENGINE,
            tab,
            width = size.width,
            height = size.height,
            "[editor] popout opened"
        );
        self.popouts.insert(
            tab.to_string(),
            PopoutWindow {
                window,
                surface,
                renderer,
                presented: false,
            },
        );
    }

    /// Refresh each popped-out window's input record: its geometry and frame
    /// rate this frame. Its events were appended as its window reported them.
    fn publish_popout_geometry(&mut self) {
        if self.popouts.is_empty() {
            return;
        }
        let geometry: Vec<(String, f32, f32, f32, bool)> = self
            .popouts
            .iter()
            .map(|(tab, popout)| {
                let scale_factor = popout.window.scale_factor() as f32;
                let size = popout.window.inner_size();
                (
                    tab.clone(),
                    size.width as f32 / scale_factor.max(0.01),
                    size.height as f32 / scale_factor.max(0.01),
                    scale_factor,
                    popout.window.has_focus(),
                )
            })
            .collect();
        let time = self.started.elapsed().as_secs_f64();
        let frames_per_second = self.fps;
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(inputs) = world.get_resource_mut::<EguiPopoutInputs>() else {
            return;
        };
        for (tab, width, height, scale_factor, focused) in geometry {
            let record = inputs.by_tab.entry(tab).or_default();
            record.screen_size = pill_core::math::Vector2f::new(width.max(1.0), height.max(1.0));
            record.scale_factor = scale_factor;
            record.focused = focused;
            record.time = time;
            record.frames_per_second = frames_per_second;
        }
    }

    /// Draw the shell's frame through the overlay; without a shell module,
    /// draw the runner's own fallback.
    fn present_ui(&mut self, event_loop: &ActiveEventLoop) {
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
            ime_allowed,
            ime_rect,
            drag_out,
            // The menu bar's quit is drained at the frame boundary (Step 7b),
            // not while the frame is presented.
            quit_requested: _,
            // egui's own cadence: the redraw decision at the end of the frame
            // honours it.
            repaint_after,
            // One frame per popped-out tab: each paints into
            // its own window, below.
            mut popout_frames,
        } = frame;
        self.repaint_after = repaint_after;
        // Each scene viewport follows its dock rectangle, so the viewports
        // track the layout when the user drags a splitter; a viewport whose
        // tab is not shown hides its window. The list is one slot per view
        // the dock holds, so a view the person just added opens here - on the
        // frame its tab first drew - and a view whose tab was closed keeps
        // its window hidden rather than dropped.
        self.ensure_scene_views(scene_rects.len(), event_loop);
        for view in 0..self.views.len() {
            match scene_rects.get(view).copied().flatten() {
                Some(rect) => {
                    // A view whose tab is back in the dock needs its child
                    // window again.
                    self.ensure_view_docked(event_loop, view);
                    self.place_view_rect(view, rect);
                }
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
        // An IME the shell asked for: the window takes the
        // candidate window, and the caret's rectangle places it. A text field
        // that loses focus turns it off again.
        self.apply_ime(ime_allowed, ime_rect);
        // A tile dragged past the window edge: the OS gets
        // the file through an OLE drag, and the editor's thread runs the modal
        // loop until the drop ends.
        if let Some(path) = drag_out {
            self.start_drag_out(&path);
        }
        if let Some(text) = clipboard_text {
            // egui's own copy (a text field's selection) leaves under the same
            // registered format the structured copy uses.
            match crate::clipboard::write(&text) {
                Ok(()) => info!(
                    target: telemetry_target::ENGINE,
                    bytes = text.len(),
                    "[editor] clipboard written"
                ),
                Err(message) => warn!(
                    target: telemetry_target::ENGINE,
                    message = message.as_str(),
                    "[editor] clipboard write failed"
                ),
            }
        }
        // Step 2b: the popped-out windows - create the ones
        // the shell reported for the first time, close the ones it stopped
        // reporting, and paint each frame into its own swapchain.
        self.present_popouts(event_loop, &mut popout_frames);
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
        // The popped-out windows' events have been through their passes: the
        // next frame starts with none.
        self.clear_popout_events();
    }

    /// The safe-mode pass, used while no shell module is loaded: the build
    /// errors that stopped it, with their `file:line`, and a Retry that asks
    /// the host to rebuild the module. The scene keeps rendering behind this
    /// panel - the editor's own frame is untouched.
    fn fallback_frame(&mut self) -> EguiShellFrame {
        // The fallback paints the same theme the shell would: a layer change
        // reaches this posture too, and a theme change is a reason to draw
        // once.
        if self.fallback_theme_generation != self.theme_generation {
            self.fallback_theme_generation = self.theme_generation;
            pill_editor_core::style::apply_theme(
                &self.fallback_context,
                &self.theme,
                &self.theme_layers,
                self.theme_generation,
                &self.theme_fonts,
            );
        }
        if !self.reported_fallback {
            self.reported_fallback = true;
            info!(
                target: telemetry_target::ENGINE,
                module = SHELL_MODULE_NAME,
                problems = self.notices.problems.len(),
                "[editor] safe mode"
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
        let mut retry = false;
        let problems: Vec<(String, String, String)> = self
            .notices
            .problems
            .iter()
            .map(|row| {
                let location = match (&row.problem.file, row.problem.line) {
                    (Some(file), Some(line)) => format!("{file}:{line}"),
                    (Some(file), None) => file.clone(),
                    _ => "-".to_string(),
                };
                (
                    row.module.clone(),
                    location,
                    format!("{} {}", row.problem.severity.name(), row.problem.message),
                )
            })
            .collect();
        let output = context.run_ui(raw_input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.heading("Pill Editor - safe mode");
                ui.label(format!(
                    "the shell module ({SHELL_MODULE_NAME}) is not loaded; the scene \
                     keeps rendering behind this panel"
                ));
                ui.separator();
                if problems.is_empty() {
                    ui.weak("no build error was reported");
                } else {
                    egui::ScrollArea::vertical()
                        .max_height(360.0)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            for (module, location, text) in &problems {
                                ui.label(egui::RichText::new(module).strong());
                                ui.label(
                                    egui::RichText::new(format!("  {location}  {text}"))
                                        .monospace(),
                                );
                            }
                        });
                }
                ui.separator();
                if ui
                    .add(egui::Button::new("Retry"))
                    .on_hover_text("rebuild and load the shell module")
                    .clicked()
                {
                    retry = true;
                }
            });
        });
        if retry {
            self.retry_shell_module();
        }
        frame_from_output(&context, output)
    }

    /// Ask the host to rebuild the shell module, and say how it went.
    ///
    /// `retry_extension` runs the same load a first start would, or asks a
    /// loaded generation to rebuild as if its sources were saved, so a fix
    /// made while the editor was in safe mode lands without a restart.
    fn retry_shell_module(&mut self) {
        let outcome = self.host.host_mut().retry_extension(SHELL_MODULE_NAME);
        match outcome {
            Ok(()) => {
                info!(
                    target: telemetry_target::ENGINE,
                    module = SHELL_MODULE_NAME,
                    "[editor] safe mode retry requested"
                );
                // The next frame looks the shell up again; a generation that
                // loaded in the meantime takes over without another step.
                self.reported_fallback = false;
            }
            Err(error) => warn!(
                target: telemetry_target::ENGINE,
                module = SHELL_MODULE_NAME,
                error = error.as_str(),
                "[editor] safe mode retry failed"
            ),
        }
    }

    /// Set the cursor shape egui asked for, when it maps onto one.
    ///
    /// The resize shapes matter as much as the pointing hand: the
    /// dock reports one over a splitter, and without it the widened grab band
    /// is invisible to the person dragging it.
    fn apply_cursor_icon(&mut self, icon: egui::CursorIcon) {
        if icon != self.last_cursor {
            info!(
                target: telemetry_target::ENGINE,
                icon = format!("{icon:?}").as_str(),
                "[editor] cursor icon"
            );
            self.last_cursor = icon;
        }
        let shape = match icon {
            egui::CursorIcon::Default => winit::window::CursorIcon::Default,
            egui::CursorIcon::PointingHand => winit::window::CursorIcon::Pointer,
            egui::CursorIcon::Text => winit::window::CursorIcon::Text,
            egui::CursorIcon::Crosshair => winit::window::CursorIcon::Crosshair,
            egui::CursorIcon::Grab => winit::window::CursorIcon::Grab,
            egui::CursorIcon::Grabbing => winit::window::CursorIcon::Grabbing,
            egui::CursorIcon::ResizeHorizontal => winit::window::CursorIcon::EwResize,
            egui::CursorIcon::ResizeVertical => winit::window::CursorIcon::NsResize,
            egui::CursorIcon::ResizeNeSw => winit::window::CursorIcon::NeswResize,
            egui::CursorIcon::ResizeNwSe => winit::window::CursorIcon::NwseResize,
            egui::CursorIcon::ResizeEast => winit::window::CursorIcon::EResize,
            egui::CursorIcon::ResizeWest => winit::window::CursorIcon::WResize,
            egui::CursorIcon::ResizeNorth => winit::window::CursorIcon::NResize,
            egui::CursorIcon::ResizeSouth => winit::window::CursorIcon::SResize,
            egui::CursorIcon::ResizeColumn => winit::window::CursorIcon::ColResize,
            egui::CursorIcon::ResizeRow => winit::window::CursorIcon::RowResize,
            _ => return,
        };
        self.window.set_cursor(shape);
    }

    /// Try to create the composition overlay again, at most every 120 frames.
    ///
    /// The window posture is not retried: it was either asked for
    /// (`PILL_EDITOR_NO_OVERLAY`) or already taken as the fallback, and a
    /// composition overlay is the posture that failed.
    fn retry_overlay(&mut self) {
        if self.no_overlay {
            return;
        }
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
    /// The grammar lives in [`pill_editor_core::scripted`]: this parses each
    /// line and dispatches it, and a line that cannot be parsed takes the one
    /// refusal path, so a suite sees it at once instead of waiting out a
    /// timeout.
    ///
    /// What a line queues is applied before the next line runs: one append
    /// reaches the loop as one batch, so without this an append that edits and
    /// then reads would answer the value the edit replaced.
    fn poll_test_channel(&mut self) {
        let lines = match &mut self.test_channel {
            Some(channel) => channel.poll(),
            None => return,
        };
        for line in lines {
            // A scripted line arrives between frames; the loop must not stay
            // asleep over it.
            self.demand.scripted = true;
            match ScriptedCommand::parse(&line) {
                Ok(command) => self.run_scripted(&line, command),
                Err(error) => self.reject_scripted(&line, &error.reason),
            }
            self.apply_pending_commands();
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
                self.request_quit();
            }
            ScriptedCommand::Undock(tab) => self.undock_tab(tab),
            ScriptedCommand::ActivateTab(tab) => self.activate_tab(tab),
            ScriptedCommand::Palette(request) => self.scripted_palette(request),
            ScriptedCommand::ConsoleFilter(level) => self.scripted_console_filter(&level),
            ScriptedCommand::ConsoleSearch(text) => self.scripted_console_search(text),
            ScriptedCommand::ProblemOpen(index) => self.scripted_problem_open(index),
            ScriptedCommand::SafeModeRetry => self.retry_shell_module(),
            ScriptedCommand::PanelUnload(source) => self.scripted_panel_unload(&source),
            ScriptedCommand::PanelRestore(source) => self.scripted_panel_restore(&source),
            ScriptedCommand::PanelWindow { slot, id } => self.scripted_panel_window(slot, &id),
            ScriptedCommand::PanelField {
                component,
                field,
                value,
            } => self.scripted_panel_field(&component, &field, &value),
            ScriptedCommand::Reveal(path) => {
                let world = self.host.host_mut().engine_mut().world_mut();
                if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
                    pending.reveal(path.clone());
                }
                info!(
                    target: telemetry_target::ENGINE,
                    path = path.as_str(),
                    "[editor] reveal queued"
                );
            }
            ScriptedCommand::PickReference {
                asset,
                key,
                candidate,
            } => {
                // The choice a picker row makes: the candidate's own guid,
                // written through the same settings edit the row queues.
                let guid = self
                    .asset_entries
                    .iter()
                    .find(|row| row.path == candidate || row.key() == candidate)
                    .and_then(|row| row.guid.clone());
                match guid {
                    Some(guid) => {
                        info!(
                            target: telemetry_target::ENGINE,
                            key = key.as_str(),
                            candidate = candidate.as_str(),
                            guid = guid.as_str(),
                            "[editor] reference pick queued"
                        );
                        self.pick_reference(&asset, &key, &guid);
                    }
                    None => self
                        .reject_scripted(line, "pick reference needs a candidate carrying a guid"),
                }
            }
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
                Dump::EngineAssets => self.dump_engine_assets(),
                Dump::Browser => self.dump_browser(),
                Dump::StandaloneTypes => self.dump_standalone_types(),
                Dump::Systems => self.dump_systems(),
                Dump::Preferences => self.dump_preferences(),
                Dump::Theme => self.dump_theme(),
                Dump::Console => self.dump_console(),
                Dump::Problems => self.dump_problems(),
                Dump::Stats => self.dump_stats(),
                Dump::Cadence => self.dump_cadence(),
                Dump::Panels => self.dump_panels(),
                Dump::Windows => self.dump_windows(),
                Dump::Resources => self.dump_resources(),
                Dump::Entities => self.dump_entities(),
                Dump::Selection => self.dump_selection(),
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
            ScriptedCommand::HoverUi { x, y } => self.scripted_hover_ui(x, y),
            ScriptedCommand::Drop(path) => self.drop_files(vec![std::path::PathBuf::from(path)]),
            ScriptedCommand::HoverFile(path) => self.preview_drop(std::path::Path::new(&path)),
            ScriptedCommand::DragOut(path) => self.start_drag_out(&path),
            ScriptedCommand::SelectAsset(path) => self.scripted_select_asset(&path),
            ScriptedCommand::SelectEntity(handle) => self.scripted_select_entity(&handle),
            ScriptedCommand::SelectAdd(handle) => {
                self.scripted_select_extend(&handle, SelectionClick::Add)
            }
            ScriptedCommand::SelectRange(handle) => {
                self.scripted_select_extend(&handle, SelectionClick::Range)
            }
            ScriptedCommand::SelectResource(name) => self.scripted_select_resource(&name),
            ScriptedCommand::EditResource {
                resource,
                field,
                value,
            } => self.scripted_edit_resource(&resource, &field, &value),
            ScriptedCommand::Toolbar(action) => self.scripted_toolbar(action),
            ScriptedCommand::SceneNew => self.scripted_scene(EditorCommand::SceneNew),
            ScriptedCommand::SceneOpen(path) => {
                self.scripted_scene(EditorCommand::SceneOpen { path })
            }
            ScriptedCommand::SceneSave(path) => match path {
                Some(path) => self.scripted_scene(EditorCommand::SceneSaveAs { path }),
                None => self.scripted_scene(EditorCommand::SceneSave),
            },
            ScriptedCommand::DialogSetName(text) => self.scripted_dialog_set_name(&text),
            ScriptedCommand::DialogSubmit => self.scripted_dialog_submit(),
            ScriptedCommand::EditAsset(edit) => self.scripted_asset_edit(edit),
            ScriptedCommand::PickEntity {
                entity,
                component,
                field,
            } => self.scripted_pick_entity(&entity, &component, &field),
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
            ScriptedCommand::EditSelection {
                component,
                field,
                value,
            } => self.scripted_edit_selection(&component, &field, &value),
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
            ScriptedCommand::Copy => self.scripted_clipboard(ClipboardAction::Copy),
            ScriptedCommand::Cut => self.scripted_clipboard(ClipboardAction::Cut),
            ScriptedCommand::Paste => self.scripted_clipboard(ClipboardAction::Paste),
            ScriptedCommand::PauseToggle => self.scripted_pause_toggle(),
            ScriptedCommand::NewSceneView => self.scripted_new_scene_view(),
            ScriptedCommand::Popout { tab, redock } => self.scripted_popout(tab, redock),
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
        // A scripted chord runs what a real press runs: the same
        // registry-resolved command, with the modifiers the chord named.
        self.run_bound_command(
            chord.key,
            EngineModifiers {
                shift: chord.shift,
                control: chord.control,
                alt: chord.alt,
                super_key: chord.super_key,
            },
        );
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
        info!(
            target: telemetry_target::ENGINE,
            count = notices.len(),
            "[editor] notices drained"
        );
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
                    // The swap means the old image is gone: its panels' draw
                    // functions are stale addresses. Dropping the marker makes
                    // them read "module not loaded" until the new generation's
                    // request arrives this same frame.
                    if self.panel_markers.remove(module).is_some() {
                        info!(
                            target: telemetry_target::ENGINE,
                            module = module.as_str(),
                            "[editor] panel source reloaded"
                        );
                    }
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

    /// Queue one toast the editor itself raised, and publish it to the shell.
    ///
    /// The host's notices arrive through `fold_host_notices`, which returns
    /// early when the host said nothing; an editor warning (a keybinding
    /// conflict, a refused drop) has to reach the same event list and the
    /// world resource on its own, or the stack would only ever show reloads.
    fn push_toast(&mut self, level: NoticeLevel, text: String) {
        let seq = self.notices.next_event_seq;
        self.notices.next_event_seq = seq + 1;
        self.notices.events.push(Notice { seq, level, text });
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

    /// Queue a scripted left-button drag: the press, the move
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
        if pill_editor_core::layout::activate_tab(state.dock.state_mut(), tab) {
            info!(
                target: telemetry_target::ENGINE,
                tab = tab.name(),
                "[editor] tab activated"
            );
        } else if tab.module_slot().is_some() {
            // A module panel starts outside the dock: the layout is the user's,
            // and a panel that registers later joins the focused leaf rather
            // than rewriting their arrangement.
            state.dock.state_mut().push_to_focused_leaf(tab);
            info!(
                target: telemetry_target::ENGINE,
                tab = tab.name(),
                "[editor] module tab docked"
            );
        } else if tab.scene_view().is_some() {
            // A scene view's tab is docked the moment it is asked for: a view
            // the person added has no other way in.
            state.dock.state_mut().push_to_focused_leaf(tab);
            pill_editor_core::layout::activate_tab(state.dock.state_mut(), tab);
            info!(
                target: telemetry_target::ENGINE,
                tab = tab.name(),
                "[editor] scene tab docked"
            );
        } else {
            // The menu named a tab the dock no longer holds: a panel the
            // person closed. Naming it is a request to show it again, so it
            // comes back to the focused leaf instead of being dropped.
            state.dock.state_mut().push_to_focused_leaf(tab);
            pill_editor_core::layout::activate_tab(state.dock.state_mut(), tab);
            info!(
                target: telemetry_target::ENGINE,
                tab = tab.name(),
                "[editor] tab docked"
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
            self.request_quit();
        }
    }

    /// Leave the editor: write the dock layout, then set the quit flag.
    ///
    /// Every way out - the menu, the scripted channel, the test deadline -
    /// comes through here, so the layout a restart loads is always the one
    /// the last session ended with.
    fn request_quit(&mut self) {
        // The dock's module tabs name slots; the ids they stood for are what
        // the next run needs, so they are written first.
        let panel_ids: Vec<String> = {
            let world = self.host.host_mut().engine_mut().world_mut();
            world
                .get_resource::<PanelRegistry>()
                .map(|registry| {
                    registry
                        .entries()
                        .iter()
                        .map(|entry| entry.id.clone())
                        .collect()
                })
                .unwrap_or_default()
        };
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(state) = world.get_resource_mut::<EguiShellState>() {
            state.dock.set_panel_ids(panel_ids);
            let surfaces = state.dock.state().surfaces_count();
            pill_editor_core::layout::save(&state.dock);
            info!(
                target: telemetry_target::ENGINE,
                surfaces,
                "[editor] layout saved"
            );
        }
        // The run is ending on purpose: there is nothing left to recover.
        pill_editor_core::layout::remove_recovery();
        self.quit = true;
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
        // One line per leaf, so a suite can tell a restored layout from the
        // default: which tabs a group holds and which of them is in front.
        let leaves: Vec<(usize, usize, String, String)> = state
            .dock
            .state()
            .iter_all_nodes()
            .filter_map(|(path, node)| match node {
                pill_editor_core::egui_dock::Node::Leaf(leaf) => {
                    let tabs = leaf
                        .tabs
                        .iter()
                        .map(|tab| tab.name())
                        .collect::<Vec<_>>()
                        .join(",");
                    let active = leaf
                        .tabs
                        .get(leaf.active.0)
                        .map_or_else(String::new, |tab| tab.name().to_string());
                    Some((path.surface.0, path.node.0, active, tabs))
                }
                _ => None,
            })
            .collect();
        for (surface, node, active, tabs) in leaves {
            info!(
                target: telemetry_target::ENGINE,
                surface,
                node,
                active = %active,
                tabs = %tabs,
                "[editor] egui dock leaf"
            );
        }
        // Popped-out tabs are not in the tree, so they get their own lines:
        // a suite reads which tabs left the dock and which came back.
        let popouts: Vec<String> = state.dock.popouts().iter().map(|tab| tab.name()).collect();
        for tab in popouts {
            info!(
                target: telemetry_target::ENGINE,
                tab = tab.as_str(),
                "[editor] egui dock popout"
            );
        }
    }
}

/// One scene view: its child window and its renderer surface.
struct SceneView {
    /// The child window the project's renderer presents the scene into.
    window: SurfaceWindow,
    /// The renderer surface id the scene draws through.
    surface_id: SurfaceId,
    /// Whether `window` was created for a popped-out tab:
    /// a top-level window the view follows to another monitor, rather than
    /// the child window the dock places. A detached window the *platform*
    /// forced is not a popped-out one, which is why the flag exists apart from
    /// `SurfaceWindow::detached`.
    popped: bool,
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
            surface_id: scene_surface(view),
            popped: false,
        })
    }
}

// =============================================================================
// Host notices to editor notices
// =============================================================================

/// Whether one frame's shell events carry a paste chord.
///
/// The clipboard commands stay un-global while a widget holds the focus, so
/// this is what makes Ctrl+V reach a text field: the chord is recognised here
/// and the OS clipboard's text rides the frame's input.
fn paste_chord(events: &[InputEvent]) -> bool {
    events.iter().any(|event| match event {
        InputEvent::Key { key, state } => *key == KeyCode::KeyV && *state == ButtonState::Pressed,
        _ => false,
    })
}

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

/// One popped-out tab's window: the window, its own
/// swapchain and its own egui renderer.
///
/// A pop-out is a plain top-level window, not a child: the point of popping a
/// panel out is putting it on another monitor, where a child window cannot go.
struct PopoutWindow {
    /// The window the pass is presented into.
    window: Arc<Window>,
    /// The window's swapchain, in the opaque window posture.
    surface: OverlaySurface,
    /// egui's renderer for this window. Its textures belong to this renderer,
    /// which is why each pop-out owns one: two renderers cannot share a
    /// texture namespace across swapchains.
    renderer: egui_wgpu::Renderer,
    /// Whether a frame has reached the swapchain yet: the first success logs
    /// the suite's `[editor] popout presented` line.
    presented: bool,
}

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
                // SAFETY: `main` is alive - the caller passes it by reference
                // and the editor owns it for the whole run - so the raw handle
                // it yields names a valid window for `create_window` below.
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

    /// Place a popped-out view's top-level window at a screen position: the
    /// pop-out owns its screen rectangle, so the view window follows it there,
    /// on whatever monitor that is.
    fn place_screen(&mut self, x: i32, y: i32, width: u32, height: u32) {
        if !self.visible {
            self.window.set_visible(true);
            self.visible = true;
        }
        if self.placed == Some((x, y, width, height)) {
            return;
        }
        self.window.set_outer_position(PhysicalPosition::new(x, y));
        // The client origin is what a pointer event from this window is
        // relative to; decorations make it differ from the outer position, so
        // it is read back once the move was applied.
        if let Ok(inner) = self.window.inner_position() {
            self.origin = (inner.x, inner.y);
        }
        let _ = self
            .window
            .request_inner_size(PhysicalSize::new(width.max(1), height.max(1)));
        self.placed = Some((x, y, width, height));
    }

    /// Create a top-level window for a popped-out scene view: decorated, so the
    /// person can move and close it like any window.
    fn open_top_level(
        event_loop: &ActiveEventLoop,
        title: &str,
    ) -> Result<SurfaceWindow, EditorError> {
        let attributes = Window::default_attributes()
            .with_title(title)
            .with_inner_size(INITIAL_SCENE_SIZE)
            .with_visible(false);
        let window = event_loop
            .create_window(attributes)
            .map_err(|source| EditorError::WindowCreation { source })?;
        Ok(SurfaceWindow {
            window: Arc::new(window),
            detached: true,
            visible: false,
            placed: None,
            origin: (0, 0),
        })
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
    /// Move the view onto `window` and re-attach its renderer surface to it:
    /// a popped-out view needs a top-level window, and a redocked one needs a
    /// child of the editor window again. The surface's previous window is
    /// dropped by the host when the new one replaces it.
    fn rebind_window(&mut self, host: &mut RenderingHost, window: SurfaceWindow, popped: bool) {
        self.window = window;
        self.popped = popped;
        let size = self.window.size();
        match host.attach_surface(
            self.surface_id,
            Arc::clone(&self.window.window),
            size.0,
            size.1,
            SurfaceOptions::default(),
        ) {
            Ok(()) => info!(
                target: telemetry_target::ENGINE,
                surface = self.surface_id.0,
                popped,
                "[editor] scene view rebound"
            ),
            Err(error) => warn!(
                target: telemetry_target::ENGINE,
                surface = self.surface_id.0,
                error = %error,
                "[editor] scene view rebind refused"
            ),
        }
    }

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
                    surface = self.surface_id.0,
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

/// Where one translated window event belongs, by the focus rule.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EventRoute {
    /// The game's window: the project's systems receive it.
    Game,
    /// One of the editor's windows: only the shell sees it, so a keystroke
    /// meant for the Inspector never drives the game.
    Shell,
}

/// The focus rule in one place: the game's window feeds the game, every
/// editor window feeds the shell.
fn route_event(_event: &InputEvent, is_game_window: bool) -> EventRoute {
    if is_game_window {
        EventRoute::Game
    } else {
        EventRoute::Shell
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
fn tab_name(tab: EditorTab) -> String {
    tab.name()
}

/// The tab whose wire name is `name`, for the frontend's side of a pop-out
/// move: the shell owns the enum, this side only knows the
/// name a window carries.
fn editor_tab_by_name(name: &str) -> Option<EditorTab> {
    if let Some(rest) = name.strip_prefix("scene") {
        if rest.is_empty() {
            return Some(EditorTab::Scene(0));
        }
        return rest
            .parse::<u16>()
            .ok()
            .and_then(|count| count.checked_sub(1))
            .and_then(|index| u8::try_from(index).ok())
            .map(EditorTab::Scene);
    }
    EditorTab::ALL.into_iter().find(|tab| tab.name() == name)
}

// =============================================================================
// EditorCamera
// =============================================================================

/// The camera binding the scene surface draws through: the view's orbit
/// camera as a camera transform, never a world entity.
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
// Gizmo helpers
// =============================================================================

/// The wire name of a tool mode, for logs and suite tokens.
fn gizmo_mode_name(mode: GizmoMode) -> &'static str {
    match mode {
        GizmoMode::Translate => "translate",
        GizmoMode::Rotate => "rotate",
        GizmoMode::Scale => "scale",
    }
}

/// What a registry command id does in this executable.
///
/// The registry owns the ids, the titles and the chords; this maps an id to
/// the action it runs, so a rebinding reaches the same code a menu row or a
/// scripted command does. A command with no action here stays inert.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommandAction {
    /// Select a gizmo tool.
    GizmoTool(GizmoMode),
    /// Step the history back one entry.
    Undo,
    /// Step the history forward one entry.
    Redo,
    /// Flip the engine's pause flag.
    PauseToggle,
    /// Advance exactly one frame.
    Step,
}

/// A state file's metadata: what a frame compares to decide whether the file
/// needs reading at all.
///
/// The editor's state files are written by hand or by another tool, so there
/// is no in-process writer to hook; the metadata is the signal. A missing file
/// stamps as the default, which differs from any file that exists, so creating
/// one is noticed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FileStamp {
    /// The file's modification time, when the filesystem reports one.
    modified: Option<std::time::SystemTime>,
    /// Its length in bytes.
    length: u64,
}

impl FileStamp {
    /// The stamp of `path`, or the default when it cannot be read.
    fn of(path: &std::path::Path) -> Self {
        std::fs::metadata(path)
            .map(|metadata| Self {
                modified: metadata.modified().ok(),
                length: metadata.len(),
            })
            .unwrap_or_default()
    }
}

/// The snapshot fields that differ between two captures, comma-joined.
///
/// The redraw cadence treats any change as a reason to draw, so this answers
/// the question that follows: which fields moved. A field that changes on its
/// own - a timer, a counter, an unordered collection - would keep the editor
/// drawing forever, and naming it in the log is how that is caught rather
/// than suspected.
///
/// This is also the change test itself: an empty answer means the
/// two captures draw the same data. The frame index and the two tiers a frame
/// feeds itself - the console and the problems - are deliberately not
/// compared, so a capture that only added log lines is not a reason to draw.
fn snapshot_fields_changed(previous: &EditorSnapshot, current: &EditorSnapshot) -> String {
    let mut changed: Vec<&str> = Vec::new();
    if previous.entities != current.entities {
        changed.push("entities");
    }
    if previous.detail != current.detail {
        changed.push("detail");
    }
    if previous.selected_details != current.selected_details {
        changed.push("selected_details");
    }
    if previous.systems != current.systems {
        changed.push("systems");
    }
    if previous.components != current.components {
        changed.push("components");
    }
    if previous.assets != current.assets {
        changed.push("assets");
    }
    if previous.engine_assets != current.engine_assets {
        changed.push("engine_assets");
    }
    if previous.standalone_types != current.standalone_types {
        changed.push("standalone_types");
    }
    if previous.asset_detail != current.asset_detail {
        changed.push("asset_detail");
    }
    if previous.paused != current.paused {
        changed.push("paused");
    }
    if previous.gizmo != current.gizmo {
        changed.push("gizmo");
    }
    if previous.grid_enabled != current.grid_enabled {
        changed.push("grid_enabled");
    }
    if previous.preferences != current.preferences {
        changed.push("preferences");
    }
    if previous.theme_generation != current.theme_generation
        || previous.theme_layers != current.theme_layers
    {
        changed.push("theme");
    }
    if previous.undo_depth != current.undo_depth || previous.redo_depth != current.redo_depth {
        changed.push("history");
    }
    if previous.scene_title != current.scene_title
        || previous.scene_open != current.scene_open
        || previous.scene_save_path != current.scene_save_path
    {
        changed.push("scene");
    }
    if previous.commands != current.commands {
        changed.push("commands");
    }
    if previous.errors != current.errors {
        changed.push("errors");
    }
    if previous.resources != current.resources {
        changed.push("resources");
    }
    changed.join(",")
}

/// Split a command line into a program and its arguments.
///
/// Whitespace separates the parts, except inside one pair of double quotes:
/// a program path with a space in it (`"C:\Program Files\...\python.exe"`)
/// has to stay one argument, or the spawn looks for `C:\Program`.
fn split_command(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for character in text.chars() {
        match character {
            '"' => quoted = !quoted,
            whitespace if whitespace.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                }
            }
            other => current.push(other),
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// The action a command id runs, if this executable implements it.
fn command_action(command_id: &str) -> Option<CommandAction> {
    match command_id {
        "editor.tool_translate" => Some(CommandAction::GizmoTool(GizmoMode::Translate)),
        "editor.tool_rotate" => Some(CommandAction::GizmoTool(GizmoMode::Rotate)),
        "editor.tool_scale" => Some(CommandAction::GizmoTool(GizmoMode::Scale)),
        "editor.undo" => Some(CommandAction::Undo),
        "editor.redo" => Some(CommandAction::Redo),
        "editor.pause_toggle" => Some(CommandAction::PauseToggle),
        "editor.step" => Some(CommandAction::Step),
        _ => None,
    }
}

/// The bindings the editor starts with when there is no keybindings file.
///
/// Unity's table, whose W/E/R the tool shortcuts use; the file's own `preset`
/// replaces it as soon as one names a preset.
fn editor_default_keybindings() -> KeybindingsFile {
    KeybindingsFile {
        preset: Some(KeymapPreset::Unity),
        ..Default::default()
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
    /// The press pixel's horizontal coordinate, in the view's physical surface
    /// pixels.
    x: f32,
    /// The press pixel's vertical coordinate, in the view's physical surface
    /// pixels.
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
/// never names them and they cannot be grabbed. They are marked as the
/// editor's chrome, which is what keeps them off a game surface - the game
/// viewport shows the project's frame, not the editor's drag. The axis guide
/// uses the handle's own colour; the displacement line is neutral so it never
/// reads as a fourth axis.
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
        chrome: true,
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

/// The guid a settings reference holds, in either spelling: the value itself
/// when the document writes a bare guid, or the object form's `guid`.
fn reference_guid(value: Option<&serde_json::Value>) -> Option<String> {
    match value? {
        serde_json::Value::String(guid) => Some(guid.clone()),
        other => other
            .get("guid")
            .and_then(|guid| guid.as_str())
            .map(str::to_owned),
    }
}

/// Ask the platform's file manager to show `target`: Windows and macOS select
/// the file beside its siblings, and the FreeDesktop helper opens the folder
/// that holds it.
///
/// A spawn the OS refuses is not worth failing a frame over: the caller has
/// already logged what it asked for, and no window is in a state to report it.
fn open_in_file_manager(target: &std::path::Path) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let native = native_path(target);
        let mut command = std::process::Command::new("explorer");
        if target.is_dir() {
            command.arg(&native);
        } else {
            // Explorer parses `/select,<path>` itself and wants the *path*
            // quoted, not the whole token: a token quoted whole is rejected and
            // explorer falls back to the default folder. A raw argument is the
            // only way to put exactly that text on the command line, since
            // Rust would otherwise quote the token as one word.
            command.raw_arg(format!("/select,\"{}\"", native.display()));
        }
        let _ = command.spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .arg("-R")
            .arg(target)
            .spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // The FreeDesktop helper takes a folder: a file's parent is what a
        // select-in-place request shows on a desktop without `-R`.
        let folder = target.parent().unwrap_or(target);
        let _ = std::process::Command::new("xdg-open").arg(folder).spawn();
    }
}

/// `target` rebuilt from its components, so its text carries the platform's
/// own separators.
///
/// A reveal target is joined from a `/`-separated relative path, and
/// `Path::display` prints the string the path was built from - so the same
/// path can read `res/materials/x.material` on Windows, which Explorer's
/// `/select,` rejects before falling back to the user's Documents folder.
fn native_path(target: &std::path::Path) -> std::path::PathBuf {
    target.components().collect()
}

/// The store's own assets that no file of the project's backs - a chain's
/// built-in passes, the renderer's default material - as the pickers offer
/// them.
///
/// A row the listing already holds is left out: the file names that same asset,
/// and two rows for one asset would be two ways to pick one thing.
fn engine_asset_rows(
    world: &pill_engine_core::World,
    listing: &[AssetListing],
) -> Vec<pill_editor_core::editor_state::EngineAsset> {
    let Some(registry) = world
        .get_resource::<pill_engine_core::ImportRegistry>()
        .cloned()
    else {
        return Vec::new();
    };
    let Some(assets) = world.get_resource::<pill_engine_core::AssetManager>() else {
        return Vec::new();
    };
    registry
        .loaded_assets(assets)
        .into_iter()
        .filter(|(_kind, guid, name)| {
            let guid = guid.map(|guid| guid.to_string());
            !listing_backs(listing, guid.as_deref(), name)
        })
        .map(
            |(kind, guid, name)| pill_editor_core::editor_state::EngineAsset {
                kind,
                name,
                guid: guid.map(|guid| guid.to_string()),
            },
        )
        .collect()
}

/// Whether the listing already shows the store asset `name` carries.
///
/// One asset is one row: a guid the file declares, the path the store holds it
/// under, or - for an asset added under a short name - a file of that name. The
/// renderer does exactly the last: it loads `defaults/pill.pbr.material` and
/// adds those bytes to the store as `pill.pbr.material`, so a picker left to
/// the raw facts offers the same material twice, once of them unreachable.
fn listing_backs(listing: &[AssetListing], guid: Option<&str>, name: &str) -> bool {
    if let Some(guid) = guid {
        return listing.iter().any(|row| row.guid.as_deref() == Some(guid));
    }
    let file_name = name.rsplit('/').next().unwrap_or(name);
    listing.iter().any(|row| {
        row.path == name
            || row.path.ends_with(&format!("/{name}"))
            || row.path.rsplit('/').next() == Some(file_name)
    })
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// A reveal's path reaches the file manager with the platform's own
    /// separators: Explorer's `/select,` refuses a `/` and opens the user's
    /// Documents folder instead.
    #[test]
    fn a_reveal_path_is_rebuilt_from_its_components() {
        let target = std::path::Path::new("res").join("materials/unlit.material");
        let native = native_path(&target);
        assert_eq!(
            native.components().count(),
            target.components().count(),
            "every segment survives"
        );
        #[cfg(target_os = "windows")]
        assert!(
            !native.to_string_lossy().contains('/'),
            "a `/` makes Explorer fall back to Documents: {native:?}"
        );
    }

    /// A store asset a listing row already backs is not offered again: the
    /// renderer's default material is the case that matters, because the file
    /// that ships it and the name the store holds it under differ.
    #[test]
    fn a_listing_row_stands_for_the_store_asset_it_backs() {
        let row = |path: &str, guid: Option<&str>| AssetListing {
            root: String::new(),
            path: path.to_string(),
            read_only: false,
            kind: "Material".to_string(),
            standalone: false,
            has_meta: guid.is_some(),
            guid: guid.map(str::to_string),
        };
        let listing = [
            row("defaults/pill.pbr.material", None),
            row("materials/unlit.material", Some("6a1e")),
            row("shaders/pbr_fragment.hlsl", Some("7b2f")),
        ];

        assert!(
            listing_backs(&listing, None, "pill.pbr.material"),
            "the short name the store holds the default material under is that file"
        );
        assert!(
            listing_backs(&listing, None, "defaults/pill.pbr.material"),
            "the store's own path is the same row"
        );
        assert!(
            listing_backs(&listing, Some("6a1e"), "anything"),
            "a guid the file declares is the row, whatever the store names it"
        );
        assert!(
            !listing_backs(&listing, None, "pill.pbr.opaque"),
            "a pass built in code has no file, so its row stays"
        );
        assert!(
            !listing_backs(&listing, Some("ffff"), "pill.pbr.skybox"),
            "a guid no file declares leaves the store's row alone"
        );
    }

    /// The focus rule: a key or click from the game's window is the game's,
    /// and one from any editor window is the shell's alone.
    #[test]
    fn only_the_game_window_feeds_the_game() {
        let key = InputEvent::Key {
            key: KeyCode::KeyA,
            state: ButtonState::Pressed,
        };
        assert_eq!(route_event(&key, true), EventRoute::Game);
        assert_eq!(route_event(&key, false), EventRoute::Shell);
        let pointer = InputEvent::CursorMoved {
            position: pill_core::math::Vector2f::new(4.0, 9.0),
        };
        assert_eq!(route_event(&pointer, true), EventRoute::Game);
        assert_eq!(route_event(&pointer, false), EventRoute::Shell);
    }

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

    /// The change detector compares the two captures field by field, so an
    /// empty answer is the "nothing to draw" one - and the tiers a frame feeds
    /// itself are not part of the comparison.
    #[test]
    fn a_snapshot_only_demands_a_frame_when_a_drawn_field_moved() {
        let snapshot = EditorSnapshot::default();
        assert_eq!(snapshot_fields_changed(&snapshot, &snapshot), "");

        // The frame index is this frame's, so two captures a frame apart are
        // the same data.
        let later = EditorSnapshot {
            revision: 42,
            ..Default::default()
        };
        assert_eq!(snapshot_fields_changed(&snapshot, &later), "");

        // The two tiers a frame itself fills are bounded-latency data: its own
        // log lines reach the console, and treating them as a reason to draw
        // would make every frame owe the next one.
        let logged = EditorSnapshot {
            console: pill_editor_core::console::ConsoleView {
                search: "render".to_string(),
                visible: 3,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(snapshot_fields_changed(&snapshot, &logged), "");

        // A field a tab draws does demand one, and is named.
        let paused = EditorSnapshot {
            paused: true,
            ..Default::default()
        };
        assert_eq!(snapshot_fields_changed(&snapshot, &paused), "paused");
        let titled = EditorSnapshot {
            scene_title: "level.scene*".to_string(),
            ..Default::default()
        };
        assert_eq!(snapshot_fields_changed(&snapshot, &titled), "scene");
    }

    /// A state file is read when its metadata moves, and not before - the
    /// preference and keybinding watchers run every drawn frame.
    #[test]
    fn a_state_file_stamp_follows_the_file() {
        let path =
            std::env::temp_dir().join(format!("pill_state_stamp_{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            FileStamp::of(&path),
            FileStamp::default(),
            "a missing file stamps as the default"
        );

        std::fs::write(&path, "{}").expect("write");
        let first = FileStamp::of(&path);
        assert_ne!(first, FileStamp::default());
        assert_eq!(
            FileStamp::of(&path),
            first,
            "an untouched file keeps its stamp"
        );

        // A rewrite that changes the length moves the stamp. A same-length one
        // inside a single timestamp tick does not - the filesystem reports the
        // same metadata - which is why the watchers also re-read every
        // `STATE_FILE_POLL_FRAMES` frames instead of trusting the stamp alone.
        std::fs::write(&path, "[ ]").expect("write");
        assert_ne!(FileStamp::of(&path), first);
        let _ = std::fs::remove_file(&path);
    }
}
