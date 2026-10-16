//! Native editor executable: the egui frontend's frame loop, window and
//! device (Stage 8).
//!
//! # Responsibilities
//!
//! - Own the `winit` event loop, the main window, the scene view's child
//!   window, and the editor's own GPU device that draws the UI (D8, D19).
//! - Host the project and its extensions - including the reloadable egui
//!   shell module - through `pill_host` (D15).
//! - Drive one world frame per redraw: push the window events into the
//!   engine input, publish [`EguiShellInput`], run the host's frame (the
//!   shell's egui pass runs inside it), then draw the shell's
//!   [`EguiShellFrame`] through the composition overlay.
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
use pill_editor_core_egui::editor_state::EditorSnapshot;
use pill_editor_core_egui::layout::EditorTab;
use pill_editor_core_egui::session::EguiShellState;
use pill_engine_core::InputEvent;
use pill_host::{HostConfig, RenderingHost};
use pill_input::winit_events::translate_window_event;
use pill_runtime::{
    CameraBinding, CameraComponent, FrameDriver, SurfaceId, SurfaceOptions, TransformComponent,
};
use pill_ui_egui::boundary::{frame_from_output, EguiShellFrame, EguiShellInput};
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

/// The scene view's surface id; the game view is the renderer's primary
/// surface, shown by the main window itself.
const SCENE_SURFACE: SurfaceId = SurfaceId(1);

/// Where the scene view's child window sits inside the main window's client
/// area, as (left fraction, top pixels, margin pixels) of the content area.
/// The docking layout's own view rectangles replace this fixed split when
/// the shell carries view rects across the boundary (follow-up work).
const SCENE_VIEW_LEFT_FRACTION: f64 = 0.55;
const SCENE_VIEW_TOP_MARGIN: f64 = 64.0;
const SCENE_VIEW_MARGIN: f64 = 8.0;

/// How much of each new frame feeds the displayed FPS, the rest being the
/// running average.
const FPS_SMOOTHING: f32 = 0.1;

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
        let is_child = editor
            .scene
            .as_ref()
            .is_some_and(|scene| window_id == scene.window.window.id());
        if !is_main && !is_child {
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
                if is_main {
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
                for event in translate_window_event(&other) {
                    editor.pending_events.push(event);
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
    /// The scene view's child window, when it could be created.
    scene: Option<SceneView>,
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
        // Step 5: the scene view's child window and its renderer surface,
        // with the editor camera it draws through.
        let camera = EditorCamera::default();
        let mut scene = SceneView::open(event_loop, &window);
        if let Some(scene) = &mut scene {
            let scene_size = scene.window.size();
            scene.attach(&mut host, &camera, scene_size);
        }
        let mut editor = EditorWindow {
            window,
            device,
            queue,
            instance,
            adapter,
            host,
            scene,
            egui_renderer,
            overlay,
            overlay_retry_frame: 0,
            overlay_failed_logged: true,
            fallback_context: egui::Context::default(),
            reported_fallback: false,
            pending_events: Vec::new(),
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

    /// Place the scene view at its fixed rectangle inside the client area.
    fn place_views(&mut self) {
        let Some(scene) = &mut self.scene else {
            return;
        };
        let (width, height) = self.size;
        let margin = (SCENE_VIEW_MARGIN * self.scale_factor as f64) as i32;
        let left = (width as f64 * SCENE_VIEW_LEFT_FRACTION) as i32;
        let top = (SCENE_VIEW_TOP_MARGIN * self.scale_factor as f64) as i32 + margin;
        let width = (width as i32 - left - margin).max(1) as u32;
        let height = (height as i32 - top - margin).max(1) as u32;
        scene.window.place(left, top, width, height);
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
            };
        }
        // Step 4: the frame's snapshot for the shell's data tabs, when the
        // shell loaded and registered its resources.
        self.publish_snapshot();
        // Step 5: run the world frame; the shell's egui pass runs inside it
        // (a registered post-update system) and leaves the frame resource
        // for Step 6.
        let report = self.host.run_one_frame().map_err(EditorError::Renderer)?;
        let _ = report;
        // Step 6: draw the shell frame through the overlay.
        self.present_ui();
        // Step 7: the scripted channel, then the auto-quit deadline.
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
    fn publish_snapshot(&mut self) {
        let engine = self.host.host_mut().engine_mut();
        if engine.world().get_resource::<EditorSnapshot>().is_none() {
            return;
        }
        let snapshot = EditorSnapshot::capture_list(engine, self.frame_index, &[], Vec::new());
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
            ime_allowed: _,
            ime_rect: _,
        } = frame;
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
                Some("dump") if parts.next() == Some("tabs") => self.dump_tabs(),
                Some(other) => warn!(
                    target: telemetry_target::ENGINE,
                    command = other,
                    "[editor] unknown scripted command"
                ),
                None => {}
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
        let index = state.dock.state_mut().add_window(vec![tab]);
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

/// The scene view's child window and its renderer surface.
struct SceneView {
    /// The child window the project's renderer presents the scene into.
    window: SurfaceWindow,
    /// The renderer surface id the scene draws through.
    surface_id: SurfaceId,
}

impl SceneView {
    /// Create the child window and attach it to the renderer.
    ///
    /// A refusal is logged and leaves the editor without a scene view.
    fn open(event_loop: &ActiveEventLoop, main: &Window) -> Option<SceneView> {
        let window = SurfaceWindow::open(event_loop, main, "Pill Editor Scene", "scene")
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
            surface_id: SCENE_SURFACE,
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
    /// The last placed rectangle in physical pixels, so redundant moves
    /// and resizes are skipped.
    placed: Option<(i32, i32, u32, u32)>,
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
                        placed: None,
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
            .with_inner_size(INITIAL_SCENE_SIZE);
        let window = event_loop
            .create_window(detached_attributes)
            .map_err(|source| EditorError::WindowCreation { source })?;
        Ok(SurfaceWindow {
            window: Arc::new(window),
            detached: true,
            placed: None,
        })
    }

    /// Place the window at the layout rectangle, in physical pixels,
    /// skipping redundant moves. The detached fallback ignores the
    /// position: it is its own top-level window.
    fn place(&mut self, x: i32, y: i32, width: u32, height: u32) {
        if self.placed == Some((x, y, width, height)) {
            return;
        }
        if !self.detached {
            self.window.set_outer_position(PhysicalPosition::new(x, y));
        }
        let _ = self
            .window
            .request_inner_size(PhysicalSize::new(width, height));
        self.placed = Some((x, y, width, height));
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

/// Parse a tab name from a scripted command.
fn parse_tab(name: &str) -> Option<EditorTab> {
    match name {
        "entities" => Some(EditorTab::Entities),
        "systems" => Some(EditorTab::Systems),
        _ => None,
    }
}

/// The tab's display name for log lines.
fn tab_name(tab: EditorTab) -> &'static str {
    match tab {
        EditorTab::Entities => "entities",
        EditorTab::Systems => "systems",
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
