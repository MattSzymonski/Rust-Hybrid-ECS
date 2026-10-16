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
use pill_editor_core_egui::editor_state::{EditorSelection, EditorSnapshot};
use pill_editor_core_egui::gizmo::{self, GizmoMode, GizmoSettings, GizmoSpace, GizmoState};
use pill_editor_core_egui::layout::{EditorTab, BOTTOM_ROW_FRACTION, SIDE_COLUMN_FRACTION};
use pill_editor_core_egui::session::EguiShellState;
use pill_engine_core::common_components::TRANSFORM_SHARED_NAME;
use pill_engine_core::debug_draw::DebugDraw;
use pill_engine_core::input::{ButtonState, MouseButton};
use pill_engine_core::Entity;
use pill_engine_core::FieldValue;
use pill_engine_core::InputEvent;
use pill_host::{HostConfig, RenderingHost};
use pill_input::winit_events::translate_window_event;
use pill_runtime::{
    CameraBinding, CameraComponent, FrameDriver, PickHit, PickRequests, PickResults,
    SelectionOutlines, SurfaceId, SurfaceOptions, TransformComponent,
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
    /// child windows contribute their own input to the engine.
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
        if !is_main && child_view.is_none() {
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
                // The scene windows' pointer positions are relative to
                // their own client areas, while the engine - which also
                // feeds the shell's egui pass - works in the main window's
                // client coordinates. The window's placed origin bridges
                // the two, so hovering and clicking the overlay strip
                // works.
                if let WindowEvent::KeyboardInput { event, .. } = &other {
                    editor.gizmo_shortcut(event);
                }
                if let Some(view) = child_view {
                    editor.scene_window_event(view, &other);
                }
                let origin = match child_view {
                    Some(view) => editor.view_origin(view),
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
    /// The main window: the shell's UI and the game view's primary surface.
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
    /// Quit after this many seconds, when the environment asks for it.
    test_seconds: Option<f64>,
    /// A quit the scripted channel or the deadline asked for.
    quit: bool,
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
        // Step 4: the host: the project, its extensions and the egui shell
        // module, with the renderer attached to this window.
        let config = HostConfig::from_environment_for_editor_frontend("egui")
            .map_err(pill_core::error::HostError::from)?;
        let mut host = pill_host::setup_rendering(config, Arc::clone(&window), size.0, size.1)?;
        // The gizmo tool's state: the frontend owns the drags, a mode
        // change comes from the shortcuts or the scripted channel, and the
        // defaults stand until one arrives.
        let world = host.host_mut().engine_mut().world_mut();
        world.register_resource::<GizmoState>();
        if world.get_resource::<GizmoState>().is_none() {
            world.insert_resource(GizmoState::default());
        }
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
        let mut editor = EditorWindow {
            window,
            device,
            queue,
            instance,
            adapter,
            host,
            views,
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
        self.host.resize(width.max(1), height.max(1));
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
        let Some(scene) = &mut self.views[view] else {
            return;
        };
        let scale = self.scale_factor as f64;
        let x = (rect.x as f64 * scale).round() as i32;
        let y = (rect.y as f64 * scale).round() as i32;
        let width = (rect.width as f64 * scale).round().max(1.0) as i32;
        let height = (rect.height as f64 * scale).round().max(1.0) as i32;
        // Keep the child inside the client area; the dock's rect can
        // overshoot the window while a split is being dragged.
        let (client_width, client_height) = (self.size.0 as i32, self.size.1 as i32);
        let x = x.clamp(0, (client_width - 1).max(0));
        let y = y.clamp(0, (client_height - 1).max(0));
        let width = width.min((client_width - x).max(1)) as u32;
        let height = height.min((client_height - y).max(1)) as u32;
        scene.window.place(x, y, width, height);
    }

    /// Hide one scene view's window: its tab is not shown this pass
    /// (another tab of its group is active, or the tab was closed).
    fn hide_view(&mut self, view: usize) {
        if let Some(scene) = &mut self.views[view] {
            scene.window.hide();
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
    /// physical pixels, for the suite's scripted clicks.
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
    /// The detail tier - the Inspector's data - is rebuilt for the current
    /// selection every frame: it is one entity's components, cheap, and it
    /// keeps the tab's values live, the same policy the native editor
    /// follows.
    fn publish_snapshot(&mut self) {
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
        if let Some(resource) = engine.world_mut().get_resource_mut::<EditorSnapshot>() {
            *resource = snapshot;
        }
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
                Some(other) => warn!(
                    target: telemetry_target::ENGINE,
                    command = other,
                    "[editor] unknown scripted command"
                ),
                None => {}
            }
        }
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
        "entities" => Some(EditorTab::Entities),
        "systems" => Some(EditorTab::Systems),
        "inspector" => Some(EditorTab::Inspector),
        "assets" => Some(EditorTab::Assets),
        _ => None,
    }
}

/// The tab's display name for log lines.
fn tab_name(tab: EditorTab) -> &'static str {
    match tab {
        EditorTab::Scene => "scene",
        EditorTab::SecondScene => "scene2",
        EditorTab::Entities => "entities",
        EditorTab::Systems => "systems",
        EditorTab::Inspector => "inspector",
        EditorTab::Assets => "assets",
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
