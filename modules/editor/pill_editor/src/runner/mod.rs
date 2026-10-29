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
    AssetDetail, AssetKind, AssetListing, AssetReferenceField, EditorCommand, EditorSelection,
    EditorSnapshot, EntityPickRequest, SelectionClick, StandaloneTypeInfo,
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
use pill_editor_core::state_watch::StateFileWatch;
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
    editor_view_surfaces, CameraBinding, CameraComponent, FrameDriver, PickHit, PickRequests,
    PickResults, SelectionOutlines, SurfaceCameras, SurfaceId, SurfaceOptions, TransformComponent,
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

/// Frames between the run's own frame-rate heartbeats, for the person reading
/// the log.
const FRAME_HEARTBEAT_FRAMES: u64 = 210;

/// The wall-clock backstop for the same heartbeat, so a slow posture still
/// reports inside a measurement window.
const FRAME_HEARTBEAT_SECONDS: f32 = 2.0;

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

/// Frames between unconditional re-reads of the editor's state files - the
/// theme's layers poll on it, and the watchers in
/// [`pill_editor_core::state_watch`] do.
const STATE_FILE_POLL_FRAMES: u64 = pill_editor_core::state_watch::STATE_FILE_POLL_FRAMES;

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

/// Owes the loop one frame now: the timer off, a redraw asked for, and the
/// loop waiting for whatever asks next.
///
/// This is the one statement of the wake rule. `window_event` owes a frame
/// twice - once for a dropped file, which skips the rest of the walk, and once
/// at the tail for everything else - and both answer here, so a changed rule
/// (a delayed wake, a second window) changes in one place.
fn owe_frame(editor: &EditorWindow, timer: &mut WakeTimer, event_loop: &ActiveEventLoop) {
    timer.clear();
    editor.window.request_redraw();
    event_loop.set_control_flow(ControlFlow::Wait);
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
                        owe_frame(editor, &mut self.timer, event_loop);
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
                    match route_event(is_game) {
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
            owe_frame(editor, &mut self.timer, event_loop);
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

// The frame's phases, the scripted channel and the asset pipeline,
// one module each: the state stays in this module.
mod assets_pipeline;
mod editor_window;
mod frame;
mod scripted_channel;
mod transform_gizmo;
mod windows;

// The window's state and its constructor, re-exported so the method
// modules above read them through `use super::*`.
use editor_window::{EditorWindow, FontSource, ScriptedDrag, ScriptedDragStage};

// =============================================================================
// EditorWindow
// =============================================================================

impl EditorWindow {
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

    /// Resolve a scripted edit's target: the entity, the registered
    /// component name and the field's current value.
    fn resolve_edit_target(
        &mut self,
        entity_text: &str,
        component_text: &str,
        field: &str,
    ) -> Result<(Entity, String, FieldValue), String> {
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(entity) = resolve_display_entity(world, entity_text) else {
            return Err(format!("no entity named '{entity_text}'"));
        };
        let Some(component) = resolve_component_name(world, component_text) else {
            return Err(format!("no component named '{component_text}'"));
        };
        let current = world
            .read_component_field(entity, &component, field)
            .map_err(|_| format!("'{component}' has no field '{field}'"))?;
        Ok((entity, component, current))
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
            .map(|row| row.kind.text().to_string())
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
        // The same write the Inspector and the scripted channel make, so a
        // panel reaches an asset or entity reference field too.
        let recorded = match inspector::field_write(entity, &component, field, &current, value_text)
        {
            Ok(recorded) => recorded,
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
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push_recorded(recorded.commands, recorded.label, Some(recorded.coalesce));
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
        // The scripted channel's own element write: one rule, one coalesce key.
        let recorded = match inspector::element_write(
            entity, &component, field, &current, index, value_text,
        ) {
            Ok(recorded) => recorded,
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
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push_recorded(recorded.commands, recorded.label, Some(recorded.coalesce));
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
        let mut snapshot =
            EditorSnapshot::capture_engine_tiers(engine, self.frame_index, &[], Vec::new());
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
            .map(|previous| previous.changed_fields(&snapshot))
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
        if self.preferences.poll(self.frame_index).is_none() {
            return;
        }
        let text = self.preferences.text().map(str::to_string);
        let preferences = text
            .as_deref()
            .and_then(|text| serde_json::from_str::<EditorPreferences>(text).ok())
            .unwrap_or_default();
        self.preferences_document = match text.as_deref() {
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
                state.previews.clear_refused();
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
        // Until the registry has applied a file, every frame re-reads it: a
        // file that could not be applied is retried rather than skipped as
        // unchanged. Once it has, the watch's own stamp rule takes over.
        let changed = if self.keybindings_applied {
            self.keybindings.poll(self.frame_index)
        } else {
            self.keybindings.poll_now()
        };
        let Some(text) = changed else {
            return;
        };
        self.keybindings_applied = true;
        let file = text
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
            .preferences
            .text()
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
                tab = tab.name(),
                "[editor] undock needs the tab in the dock"
            );
            return;
        };
        let rect = egui::Rect::from_min_size(egui::pos2(220.0, 160.0), egui::vec2(360.0, 480.0));
        let index = dock.detach_tab(location, rect);
        info!(
            target: telemetry_target::ENGINE,
            tab = tab.name(),
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

    /// Place the window at a rectangle in physical pixels, showing it if it
    /// was hidden and skipping redundant moves.
    ///
    /// `screen_owned` is a popped-out view's window: the pop-out owns a screen
    /// rectangle on whatever monitor it is on, so the position always applies
    /// and the client origin is read back after the move, because decorations
    /// make it differ from the outer position. A docked view instead ignores
    /// the position when the platform forced it to the detached fallback - it
    /// is its own top-level window then - and its origin is the rectangle's
    /// own corner.
    fn place(&mut self, x: i32, y: i32, width: u32, height: u32, screen_owned: bool) {
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
        if screen_owned || !self.detached {
            self.window.set_outer_position(PhysicalPosition::new(x, y));
        }
        // The client origin is what a pointer event from this window is
        // relative to: a docked view's is the rectangle's corner, and a
        // screen-owned one's is read back. Both are recorded outside the
        // placement cache, because a resize clears the cache and never moves
        // the window.
        if screen_owned {
            if let Ok(inner) = self.window.inner_position() {
                self.origin = (inner.x, inner.y);
            }
        } else if !self.detached {
            self.origin = (x, y);
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
    /// Attach the view's surface to the window it has now and publish the
    /// view's camera binding for that surface.
    ///
    /// The one attach path: the startup window, a popped-out top-level and a
    /// redocked child all come through here, so the camera publish cannot be
    /// forgotten on one of them - which is what happened while the startup and
    /// rebind copies were separate.
    fn attach_to(&mut self, host: &mut RenderingHost, camera: &OrbitCamera, rebound: bool) {
        let size = self.window.size();
        match host.attach_surface(
            self.surface_id,
            Arc::clone(&self.window.window),
            size.0,
            size.1,
            SurfaceOptions::default(),
        ) {
            Ok(()) => {
                host.set_surface_camera(self.surface_id, Some(camera_binding(camera)));
                if rebound {
                    info!(
                        target: telemetry_target::ENGINE,
                        surface = self.surface_id.0,
                        popped = self.popped,
                        "[editor] scene view rebound"
                    );
                } else {
                    info!(
                        target: telemetry_target::ENGINE,
                        surface = self.surface_id.0,
                        detached = self.window.detached,
                        "[editor] scene surface attached"
                    );
                }
            }
            Err(error) => warn!(
                target: telemetry_target::ENGINE,
                surface = self.surface_id.0,
                error = %error,
                "[editor] scene surface refused; the scene view stays empty"
            ),
        }
    }

    /// Attach the view to the window it was opened with.
    fn attach(&mut self, host: &mut RenderingHost, camera: &OrbitCamera) {
        self.attach_to(host, camera, false);
    }

    /// Move the view onto `window` and attach to it: a popped-out view needs a
    /// top-level window, and a redocked one needs a child of the editor window
    /// again. The surface's previous window is dropped by the host when the new
    /// one replaces it.
    fn rebind_window(
        &mut self,
        host: &mut RenderingHost,
        window: SurfaceWindow,
        popped: bool,
        camera: &OrbitCamera,
    ) {
        self.window = window;
        self.popped = popped;
        self.attach_to(host, camera, true);
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
///
/// The event is not a parameter because it cannot matter: routing is decided
/// by the window a click or key came through, and a signature that took the
/// event would promise otherwise.
fn route_event(is_game_window: bool) -> EventRoute {
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
/// The theme's layers keep their own stamps - they name a selection to
/// re-resolve rather than a document to read - and the rule itself lives in
/// [`pill_editor_core::state_watch`], where the two plain watchers share it.
type FileStamp = pill_editor_core::state_watch::FileStamp;

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
    /// The transform field the last move wrote and the values it wrote there:
    /// the release turns these into one recorded step.
    written: Option<(&'static str, Vec<f32>)>,
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
/// One asset is one row: a guid the file declares, or the path the store holds
/// it under. A code-built asset has neither and keeps its own row.
fn listing_backs(listing: &[AssetListing], guid: Option<&str>, name: &str) -> bool {
    if let Some(guid) = guid {
        return listing.iter().any(|row| row.guid.as_deref() == Some(guid));
    }
    listing
        .iter()
        .any(|row| row.path == name || row.path.ends_with(&format!("/{name}")))
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
            kind: AssetKind::parse("Material"),
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
        assert_eq!(route_event(true), EventRoute::Game);
        assert_eq!(route_event(false), EventRoute::Shell);
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
        assert_eq!(snapshot.changed_fields(&snapshot), "");

        // The frame index is this frame's, so two captures a frame apart are
        // the same data.
        let later = EditorSnapshot {
            revision: 42,
            ..Default::default()
        };
        assert_eq!(snapshot.changed_fields(&later), "");

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
        assert_eq!(snapshot.changed_fields(&logged), "");

        // A field a tab draws does demand one, and is named.
        let paused = EditorSnapshot {
            paused: true,
            ..Default::default()
        };
        assert_eq!(snapshot.changed_fields(&paused), "paused");
        let titled = EditorSnapshot {
            scene_title: "level.scene*".to_string(),
            ..Default::default()
        };
        assert_eq!(snapshot.changed_fields(&titled), "scene");
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
