//! The egui editor in the browser (W-0029): the game on one canvas, the
//! egui shell's UI on a transparent canvas above it.
//!
//! # Responsibilities
//!
//! - [`run_editor_egui`]: run a static project that links the egui shell,
//!   put the scene on one canvas and the shell's epaint output on a canvas
//!   above it, and drive the shell's plain-data boundary
//!   (`EguiShellInput`/`EguiShellFrame`) the way the native executable does.
//! - One frame per redraw: engine input, the boundary input, the entity and
//!   system snapshot, the world frame (the shell's egui pass runs inside it),
//!   the queued command batches, then the UI draw.
//! - [`editor_command`]: the scripted channel's entry point, the web
//!   counterpart of `PILL_EDITOR_TEST_COMMANDS`.
//!
//! # Design
//!
//! The shell crate registers itself when the bundle calls its `register`
//! (its system runs inside every engine update); this module owns the
//! browser-only half. The scene canvas carries the project's own renderer,
//! attached asynchronously like the game-only runner. The UI canvas gets its
//! own `wgpu` device and an `egui-wgpu` renderer, because egui's meshes are
//! plain data and the UI must not depend on the project's renderer, which
//! can reload or fail (D8, D20).
//!
//! The UI canvas stays transparent where the dock draws nothing, and the page
//! stacks it above the scene canvas, so the scene shows through the
//! viewport holes - the browser counterpart of the native composition
//! overlay (D24, D34). There is one scene view in this stage; per-view
//! canvas pairs are follow-up work.

// Standard library
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

// External crates
use pill_core::platform::Instant;
use pill_core::telemetry::telemetry_target;
use pill_core::{error, info, warn};
use pill_editor_core_egui::commands::{ensure_command_resources, PendingEditorCommands};
use pill_editor_core_egui::drain::{self, HistoryDirection};
use pill_editor_core_egui::editor_state::{EditorCommand, EditorSelection, EditorSnapshot};
use pill_editor_core_egui::gizmo::{GizmoMode, GizmoSettings, GizmoSpace, GizmoState};
use pill_editor_core_egui::scripted::{Dump, ScriptedCommand};
use pill_editor_core_egui::{entities, systems};
use pill_engine_core::input::InputEvent;
use pill_input::winit_events::translate_window_event;
use pill_runtime::{FrameDriver, RenderingRuntime, Runtime, StaticProject};
use pill_ui_egui::boundary::{EguiShellFrame, EguiShellInput};
use web_sys::HtmlCanvasElement;
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::platform::web::{EventLoopExtWebSys, WindowAttributesExtWebSys};
use winit::window::{Window, WindowId};

// Current crate
use crate::scripted::take_scripted_lines;
use crate::{find_canvas, focus_without_scrolling, physical_size, report_failure};

/// How much of each new frame feeds the displayed FPS, the rest being the
/// running average: the same smoothing the native runner uses.
const FPS_SMOOTHING: f32 = 0.1;

/// Frames between console heartbeats, for the person reading the log and
/// the scripted suite.
const FRAME_HEARTBEAT_FRAMES: u64 = 60;

/// Run `project` as the egui editor in the page: the scene on the canvas
/// named `scene_canvas_id`, the shell's UI on the canvas named
/// `ui_canvas_id`.
///
/// Returns once the event loop is handed to the browser; the editor then
/// runs on the scene's animation frames. Every failure is reported to the
/// browser console.
pub fn run_editor_egui(project: StaticProject, scene_canvas_id: &str, ui_canvas_id: &str) {
    // Step 1: readable panics and console logging, before anything can fail.
    console_error_panic_hook::set_once();
    if let Err(error) = pill_runtime::init_telemetry(None) {
        web_sys::console::error_1(&format!("[web] telemetry failed to start: {error}").into());
    }

    // Step 2: register the linked modules - the egui shell among them - and
    // the project.
    let runtime = match pill_runtime::setup(project) {
        Ok(runtime) => runtime,
        Err(error) => return report_failure("project setup", &error),
    };

    // Step 3: hand the event loop to the browser; the canvases and the
    // renderers follow in `resumed`.
    let event_loop = match EventLoop::new() {
        Ok(event_loop) => event_loop,
        Err(error) => return report_failure("event loop creation", &error),
    };
    info!(
        target: telemetry_target::ENGINE,
        scene = scene_canvas_id,
        ui = ui_canvas_id,
        "starting the editor in the browser"
    );
    event_loop.spawn_app(WebEditorApplication {
        scene_canvas_id: scene_canvas_id.to_owned(),
        ui_canvas_id: ui_canvas_id.to_owned(),
        runtime: Some(runtime),
        scene_window: None,
        ui_window: None,
        driver: Rc::new(RefCell::new(None)),
        ui_surface: Rc::new(RefCell::new(None)),
        pending_events: Vec::new(),
        focused: true,
        started: Instant::now(),
        frames_per_second: 0.0,
        last_frame_time: None,
        frame_index: 0,
    });
}

/// The UI canvas's own device, surface and egui renderer.
struct EditorUiSurface {
    /// The swapchain the UI is drawn into.
    surface: wgpu::Surface<'static>,
    /// The swapchain's configuration, kept for resizes.
    configuration: wgpu::SurfaceConfiguration,
    /// egui's meshes drawn through this device.
    renderer: egui_wgpu::Renderer,
    /// The device the surface and renderer were created on.
    device: wgpu::Device,
    /// The queue of that device.
    queue: wgpu::Queue,
}

/// State `winit` keeps for the lifetime of the page.
struct WebEditorApplication {
    /// The element id of the canvas the game draws on.
    scene_canvas_id: String,
    /// The element id of the canvas the shell's UI draws on.
    ui_canvas_id: String,
    /// The started project, until the scene window exists to attach it to.
    runtime: Option<Runtime>,
    /// The scene canvas's window.
    scene_window: Option<Arc<Window>>,
    /// The UI canvas's window: input and size only, no renderer of its own.
    ui_window: Option<Arc<Window>>,
    /// The scene's rendering driver, filled in when the attach resolves.
    driver: Rc<RefCell<Option<RenderingRuntime>>>,
    /// The UI canvas's device, surface and renderer, filled in when its own
    /// attach resolves.
    ui_surface: Rc<RefCell<Option<EditorUiSurface>>>,
    /// UI input events since the last frame, in physical pixels.
    pending_events: Vec<InputEvent>,
    /// Whether the UI window has keyboard focus.
    focused: bool,
    /// The editor's monotonic clock origin, for the shell's input.
    started: Instant,
    /// The scene's smoothed frame rate, for the shell's overlays.
    frames_per_second: f32,
    /// When the previous frame ran, for the frame-rate smoothing.
    last_frame_time: Option<Instant>,
    /// Frames presented since startup.
    frame_index: u64,
}

impl ApplicationHandler for WebEditorApplication {
    /// Create both canvas windows and start the two asynchronous attaches.
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let Some(runtime) = self.runtime.take() else {
            return;
        };

        // Step 1: the scene canvas, exactly like the game-only runner.
        let scene_canvas = match find_canvas(&self.scene_canvas_id) {
            Ok(canvas) => canvas,
            Err(message) => {
                error!(target: telemetry_target::ENGINE, "{message}");
                return;
            }
        };
        let laid_out = physical_size(&scene_canvas);
        let attributes = Window::default_attributes()
            .with_canvas(Some(scene_canvas))
            .with_focusable(false);
        let scene_window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(error) => return report_failure("scene canvas window creation", &error),
        };
        self.scene_window = Some(Arc::clone(&scene_window));

        // Step 2: the UI canvas above it, with its own window for input.
        let ui_canvas = match find_canvas(&self.ui_canvas_id) {
            Ok(canvas) => canvas,
            Err(message) => {
                error!(target: telemetry_target::ENGINE, "{message}");
                return;
            }
        };
        let focus_target = ui_canvas.clone();
        let ui_laid_out = physical_size(&ui_canvas);
        let ui_attributes = Window::default_attributes()
            .with_canvas(Some(ui_canvas.clone()))
            .with_focusable(true);
        let ui_window = match event_loop.create_window(ui_attributes) {
            Ok(window) => Arc::new(window),
            Err(error) => return report_failure("UI canvas window creation", &error),
        };
        if let Err(error) = focus_without_scrolling(&focus_target) {
            warn!(
                target: telemetry_target::ENGINE,
                error = ?error,
                "could not focus the UI canvas; click it to send keys"
            );
        }
        self.ui_window = Some(Arc::clone(&ui_window));

        // Step 3: attach the game renderer to the scene canvas.
        let size = scene_window.inner_size();
        let (width, height) = if size.width > 0 && size.height > 0 {
            (size.width, size.height)
        } else {
            laid_out
        };
        let driver = Rc::clone(&self.driver);
        pill_core::platform::futures::spawn(async move {
            let attach =
                pill_runtime::attach_renderer(runtime, Arc::clone(&scene_window), width, height);
            match attach.await {
                Ok(mut attached) => {
                    // A resize that arrived during the attach went nowhere;
                    // apply the canvas's size as winit now measures it.
                    let size = scene_window.inner_size();
                    if size.width > 0
                        && size.height > 0
                        && (size.width, size.height) != (width, height)
                    {
                        attached.resize(size.width, size.height);
                    }
                    *driver.borrow_mut() = Some(attached);
                    info!(target: telemetry_target::RENDERING, "renderer attached to the scene canvas");
                    scene_window.request_redraw();
                }
                Err(error) => report_failure("renderer attachment", &error),
            }
        });

        // Step 4: build the UI canvas's own device and egui renderer above
        // the scene.
        let ui_surface = Rc::clone(&self.ui_surface);
        pill_core::platform::futures::spawn(async move {
            match attach_ui_surface(ui_canvas, ui_laid_out).await {
                Ok(surface) => {
                    info!(target: telemetry_target::RENDERING, "UI renderer attached to the UI canvas");
                    *ui_surface.borrow_mut() = Some(surface);
                    ui_window.request_redraw();
                }
                Err(message) => {
                    error!(target: telemetry_target::RENDERING, "{message}");
                }
            }
        });
    }

    /// Route each window's events: the scene window drives the game, the UI
    /// window collects the shell's input.
    fn window_event(&mut self, _event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let ui_window = self.ui_window.as_ref();
        if ui_window.is_some_and(|window| window.id() == id) {
            match event {
                WindowEvent::Focused(focused) => self.focused = focused,
                WindowEvent::Resized(size) => {
                    if let Some(surface) = self.ui_surface.borrow_mut().as_mut() {
                        surface.resize(size.width, size.height);
                    }
                }
                WindowEvent::RedrawRequested => self.frame(),
                other => {
                    for input in translate_window_event(&other) {
                        self.pending_events.push(input);
                    }
                }
            }
            return;
        }
        for input in translate_window_event(&event) {
            if let Some(driver) = self.driver.borrow_mut().as_mut() {
                driver.push_input(input);
            }
        }
        match event {
            WindowEvent::Resized(size) => {
                if let Some(driver) = self.driver.borrow_mut().as_mut() {
                    driver.resize(size.width, size.height);
                }
            }
            WindowEvent::RedrawRequested => self.frame(),
            _ => {}
        }
    }

    /// Forward raw mouse motion from the UI window's device stream.
    fn device_event(&mut self, _event_loop: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        if let Some(input) = pill_input::winit_events::translate_device_event(&event) {
            self.pending_events.push(input);
        }
    }
}

impl WebEditorApplication {
    /// One editor frame: input, boundary input, snapshot, the world frame
    /// (the shell's egui pass runs inside it), the command drain, then the
    /// UI draw.
    fn frame(&mut self) {
        let Some(scene_window) = self.scene_window.clone() else {
            return;
        };
        self.frame_index += 1;

        // Step 1: the smoothed frame rate, for the shell's overlays.
        let now = Instant::now();
        if let Some(last) = self.last_frame_time {
            let delta_time = now.duration_since(last).as_secs_f32();
            if delta_time > 0.0 {
                let instant = 1.0 / delta_time;
                self.frames_per_second = if self.frames_per_second > 0.0 {
                    self.frames_per_second * (1.0 - FPS_SMOOTHING) + instant * FPS_SMOOTHING
                } else {
                    instant
                };
            }
        }
        self.last_frame_time = Some(now);

        // Step 2: the UI window's events become engine input; the shell's
        // egui pass reads the same stream.
        let events: Vec<InputEvent> = std::mem::take(&mut self.pending_events);
        let scripted = take_scripted_lines();
        let ui_surface = Rc::clone(&self.ui_surface);
        let mut driver_slot = self.driver.borrow_mut();
        let Some(driver) = driver_slot.as_mut() else {
            return;
        };
        for event in &events {
            driver.push_input(event.clone());
        }

        // Step 3: the resources the executable owns natively - the gizmo's
        // state and the command registry/inbox/pending sets - exist before
        // the shell's first pass reads them.
        {
            let world = driver.engine_mut().world_mut();
            ensure_command_resources(world);
            world.register_resource::<GizmoState>();
            if world.get_resource::<GizmoState>().is_none() {
                world.insert_resource(GizmoState::default());
            }
        }

        // Step 4: apply the batches the shell queued last frame - the frame
        // boundary `EditorCommand` names - then publish this frame's
        // boundary input and snapshot.
        apply_pending_commands(
            driver.engine_mut(),
            now.duration_since(self.started),
            &scripted,
        );
        let scale_factor = self
            .ui_window
            .as_ref()
            .map_or(1.0, |window| window.scale_factor() as f32)
            .max(f32::EPSILON);
        let logical_size =
            self.ui_window
                .as_ref()
                .map_or(pill_core::math::Vector2f::new(1.0, 1.0), |window| {
                    let size = window.inner_size();
                    pill_core::math::Vector2f::new(size.width as f32, size.height as f32)
                        / scale_factor
                });
        {
            let world = driver.engine_mut().world_mut();
            if let Some(input) = world.get_resource_mut::<EguiShellInput>() {
                *input = EguiShellInput {
                    screen_size: logical_size,
                    scale_factor,
                    focused: self.focused,
                    time: now.duration_since(self.started).as_secs_f64(),
                    frames_per_second: self.frames_per_second,
                    // The browser's window is the whole editor, so every
                    // event is the shell's as well as the game's (W-0029's
                    // behaviour; the split rule is W-0038's, Stage 1).
                    events: events.clone(),
                };
            }
        }
        publish_snapshot(driver.engine_mut(), self.frame_index);

        // Step 5: run the world frame; the shell's egui pass runs inside it
        // and leaves `EguiShellFrame` for Step 6.
        match driver.run_frame() {
            Ok(report) => {
                if let Some(report) = report {
                    self.frames_per_second = report.fps as f32;
                }
            }
            Err(error) => {
                report_failure("frame render", &error);
                return;
            }
        }

        // Step 6: draw the shell's frame into the UI canvas.
        draw_ui(&ui_surface, driver.engine());
        if self.frame_index.is_multiple_of(FRAME_HEARTBEAT_FRAMES) {
            // NOTE: asserted on by the browser suite.
            let (entities, systems) = driver
                .engine()
                .world()
                .get_resource::<EditorSnapshot>()
                .map_or((0, 0), |snapshot| {
                    (snapshot.entities.len(), snapshot.systems.len())
                });
            info!(
                target: telemetry_target::ENGINE,
                frames = self.frame_index,
                entities,
                systems,
                "[editor] web frame"
            );
        }
        scene_window.request_redraw();
    }
}

/// Apply the command batches the shell queued last frame.
///
/// The scripted lines go through the shared grammar
/// ([`pill_editor_core_egui::scripted`]) and then the shared core drain, so
/// the browser parses, refuses and applies exactly what the desktop editor
/// does - including the history recording, the remap and the selection
/// follow. The listing flag has no work here: the browser stages no asset
/// panel yet.
fn apply_pending_commands(
    engine: &mut pill_engine_core::Engine,
    elapsed: std::time::Duration,
    scripted: &[String],
) {
    run_scripted_lines(engine, scripted);
    let now_millis = elapsed.as_millis() as u64;
    let _ = drain::apply_pending(engine, now_millis);
}

/// Run the scripted lines the page appended, through the shared grammar.
///
/// A line the grammar refuses takes the same `[editor] scripted command
/// rejected` path the desktop editor's channel uses, so a suite that drives
/// the page sees a refusal instead of a wait.
fn run_scripted_lines(engine: &mut pill_engine_core::Engine, lines: &[String]) {
    for line in lines {
        match ScriptedCommand::parse(line) {
            Ok(command) => run_scripted(engine, line, command),
            Err(error) => warn!(
                target: telemetry_target::ENGINE,
                command = line.as_str(),
                reason = error.reason.as_str(),
                "[editor] scripted command rejected"
            ),
        }
    }
}

/// Run one parsed command the browser posture carries.
///
/// The browser has no window chrome, no asset panel and no picking yet, so
/// the verbs that need them are refused by name: a vocabulary that silently
/// ignores a line is worse than one that says which verb is missing.
fn run_scripted(engine: &mut pill_engine_core::Engine, line: &str, command: ScriptedCommand) {
    match command {
        ScriptedCommand::Undo => {
            drain::step_history(engine, HistoryDirection::Undo);
        }
        ScriptedCommand::Redo => {
            drain::step_history(engine, HistoryDirection::Redo);
        }
        ScriptedCommand::PauseToggle => {
            let paused = engine.systems_paused();
            if let Some(pending) = engine
                .world_mut()
                .get_resource_mut::<PendingEditorCommands>()
            {
                pending.push(vec![systems::pause_toggle_command(paused)]);
            }
            info!(
                target: telemetry_target::ENGINE,
                paused = !paused,
                "[editor] pause toggled"
            );
        }
        ScriptedCommand::Step => {
            if let Some(pending) = engine
                .world_mut()
                .get_resource_mut::<PendingEditorCommands>()
            {
                pending.push(vec![EditorCommand::StepOnce]);
            }
            info!(target: telemetry_target::ENGINE, "[editor] step queued");
        }
        ScriptedCommand::EntityCreate => {
            let (command, label) = entities::entity_create();
            if let Some(pending) = engine
                .world_mut()
                .get_resource_mut::<PendingEditorCommands>()
            {
                pending.push_recorded(vec![command], label, None);
            }
            info!(target: telemetry_target::ENGINE, "[editor] new entity queued");
        }
        ScriptedCommand::EntityDelete => {
            let (entity, asset) = engine
                .world()
                .get_resource::<EditorSelection>()
                .map(|selection| (selection.entity, selection.asset.clone()))
                .unwrap_or((None, None));
            let Some(entity) = entity else {
                info!(
                    target: telemetry_target::ENGINE,
                    "[editor] delete entity skipped nothing selected"
                );
                return;
            };
            let (commands, label) = entities::entity_delete(entity, asset);
            if let Some(pending) = engine
                .world_mut()
                .get_resource_mut::<PendingEditorCommands>()
            {
                pending.push_recorded(commands, label, None);
            }
            info!(
                target: telemetry_target::ENGINE,
                entity = %entity,
                "[editor] delete entity queued"
            );
        }
        ScriptedCommand::GizmoTool(mode) => {
            queue_gizmo_tool(engine, Some(mode), None, None);
        }
        ScriptedCommand::SnapToggle => {
            let snap = gizmo_settings(engine).snap;
            queue_gizmo_tool(engine, None, None, Some(!snap));
        }
        ScriptedCommand::SpaceToggle => {
            let space = match gizmo_settings(engine).space {
                GizmoSpace::World => GizmoSpace::Local,
                GizmoSpace::Local => GizmoSpace::World,
            };
            queue_gizmo_tool(engine, None, Some(space), None);
        }
        // The page's data channel: the entity and system counts the shell's
        // tabs list, which is what the browser's probe reads.
        ScriptedCommand::Dump(Dump::Tabs) => {
            let (entities, systems) = engine
                .world()
                .get_resource::<EditorSnapshot>()
                .map_or((0, 0), |snapshot| {
                    (snapshot.entities.len(), snapshot.systems.len())
                });
            info!(
                target: telemetry_target::ENGINE,
                entities,
                systems,
                "[editor] web rows"
            );
        }
        other => refuse_in_browser(line, &other),
    }
}

/// Refuse a command this posture has no code for, naming the line and the
/// verb so the gap is readable in the page's console.
fn refuse_in_browser(line: &str, command: &ScriptedCommand) {
    let reason = format!(
        "'{}' is not available in the browser editor yet (W-0029)",
        scripted_name(command)
    );
    warn!(
        target: telemetry_target::ENGINE,
        command = line,
        reason = reason.as_str(),
        "[editor] scripted command rejected"
    );
}

/// The wire name of one parsed command, for a refusal line.
fn scripted_name(command: &ScriptedCommand) -> &'static str {
    match command {
        ScriptedCommand::Quit => "quit",
        ScriptedCommand::Undock(_) => "undock",
        ScriptedCommand::ActivateTab(_) => "tab",
        ScriptedCommand::Click { .. } => "click",
        ScriptedCommand::RightClick { .. } => "rclick",
        ScriptedCommand::Drag { .. } => "drag",
        ScriptedCommand::GizmoDrag { .. } => "gizmo drag",
        ScriptedCommand::GizmoPress { .. } => "gizmo press",
        ScriptedCommand::GizmoRelease => "gizmo release",
        ScriptedCommand::GizmoTool(_) => "gizmo",
        ScriptedCommand::Key(_) => "key",
        ScriptedCommand::Dump(_) => "dump",
        ScriptedCommand::Browser(_) => "browser",
        ScriptedCommand::Camera { .. } => "camera",
        ScriptedCommand::CameraZoom(_) => "camera zoom",
        ScriptedCommand::Hover { .. } => "hover",
        ScriptedCommand::SelectAsset(_) => "select asset",
        ScriptedCommand::SelectEntity(_) => "select entity",
        ScriptedCommand::Toolbar(_) => "toolbar",
        ScriptedCommand::DialogSetName(_) => "dialog set name",
        ScriptedCommand::DialogSubmit => "dialog submit",
        ScriptedCommand::EditAsset(_) => "edit asset",
        ScriptedCommand::EditSet { .. } => "edit set",
        ScriptedCommand::EditArray { .. } => "edit array",
        ScriptedCommand::EditAssetSettings { .. } => "edit asset-settings",
        ScriptedCommand::EditSystem { .. } => "edit system",
        ScriptedCommand::EditPreference { .. } => "edit preference",
        ScriptedCommand::EditComponent { .. } => "edit add/remove",
        ScriptedCommand::Undo => "command editor.undo",
        ScriptedCommand::Redo => "command editor.redo",
        ScriptedCommand::PauseToggle => "command editor.pause_toggle",
        ScriptedCommand::Step => "command editor.step",
        ScriptedCommand::EntityCreate => "command editor.entity_create",
        ScriptedCommand::EntityDelete => "command editor.entity_delete",
        ScriptedCommand::SnapToggle => "command editor.tool_snap_toggle",
        ScriptedCommand::SpaceToggle => "command editor.tool_space_toggle",
    }
}

/// The gizmo settings the world holds, defaults when none was registered.
fn gizmo_settings(engine: &pill_engine_core::Engine) -> GizmoSettings {
    engine
        .world()
        .get_resource::<GizmoState>()
        .map(|state| state.settings)
        .unwrap_or_default()
}

/// Queue one `SetGizmoTool` edit: the same control edit the desktop editor's
/// menu, shortcuts and scripted channel queue.
fn queue_gizmo_tool(
    engine: &mut pill_engine_core::Engine,
    mode: Option<GizmoMode>,
    space: Option<GizmoSpace>,
    snap: Option<bool>,
) {
    if let Some(pending) = engine
        .world_mut()
        .get_resource_mut::<PendingEditorCommands>()
    {
        pending.push(vec![EditorCommand::SetGizmoTool { mode, space, snap }]);
    }
}

/// Capture the entity and system tiers for the shell's data tabs.
///
/// The asset listing, standalone types and preferences document stay at
/// their defaults until the browser stages that own them land (W-0029
/// Stage 2).
fn publish_snapshot(engine: &mut pill_engine_core::Engine, frame_index: u64) {
    let selected = engine
        .world()
        .get_resource::<EditorSelection>()
        .and_then(|selection| selection.entity);
    let mut snapshot = EditorSnapshot::capture_list(engine, frame_index, &[], Vec::new());
    snapshot.detail = selected.and_then(|entity| EditorSnapshot::capture_detail(engine, entity));
    if let Some(resource) = engine.world_mut().get_resource_mut::<EditorSnapshot>() {
        *resource = snapshot;
    }
}

/// Draw the shell's frame into the UI canvas: apply its texture deltas, then
/// draw the meshes over a fully transparent clear.
fn draw_ui(ui_surface: &Rc<RefCell<Option<EditorUiSurface>>>, engine: &pill_engine_core::Engine) {
    let mut ui_surface = ui_surface.borrow_mut();
    let Some(ui) = ui_surface.as_mut() else {
        return;
    };
    let world = engine.world();
    let Some(frame) = world.get_resource::<EguiShellFrame>() else {
        return;
    };
    let texture = match ui.surface.get_current_texture() {
        Ok(texture) => texture,
        Err(error) => {
            warn!(target: telemetry_target::RENDERING, error = ?error, "UI canvas surface unavailable");
            return;
        }
    };
    let view = texture
        .texture
        .create_view(&wgpu::TextureViewDescriptor::default());
    let screen_descriptor = egui_wgpu::ScreenDescriptor {
        size_in_pixels: [ui.configuration.width, ui.configuration.height],
        pixels_per_point: frame.pixels_per_point.max(0.01),
    };
    // Step 1: the texture updates before anything samples them.
    for (texture_id, image_delta) in &frame.textures.set {
        ui.renderer
            .update_texture(&ui.device, &ui.queue, *texture_id, image_delta);
    }
    // Step 2: the draw commands, its own command buffers submitted with the
    // pass.
    let mut encoder = ui
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("pill editor egui"),
        });
    let user_buffers = ui.renderer.update_buffers(
        &ui.device,
        &ui.queue,
        &mut encoder,
        &frame.meshes,
        &screen_descriptor,
    );
    {
        let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("pill editor egui pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                resolve_target: None,
                ops: wgpu::Operations {
                    // Transparent where the dock drew nothing: the scene
                    // canvas below shows through the viewport holes.
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        // The lifetime erasure extends the pass's borrows to `'static`;
        // it stays inside this block, so nothing outlives `view`.
        let mut pass = pass.forget_lifetime();
        ui.renderer
            .render(&mut pass, &frame.meshes, &screen_descriptor);
    }
    // Step 3: release the textures the shell asked to free, then submit.
    for texture_id in &frame.textures.free {
        ui.renderer.free_texture(texture_id);
    }
    ui.queue
        .submit(user_buffers.into_iter().chain(Some(encoder.finish())));
    texture.present();
}

impl EditorUiSurface {
    /// Resize the swapchain to the UI window's physical extent.
    fn resize(&mut self, width: u32, height: u32) {
        let width = width.max(1);
        let height = height.max(1);
        if self.configuration.width == width && self.configuration.height == height {
            return;
        }
        self.configuration.width = width;
        self.configuration.height = height;
        self.surface.configure(&self.device, &self.configuration);
    }
}

/// Build the UI canvas's device, surface and egui renderer.
async fn attach_ui_surface(
    canvas: HtmlCanvasElement,
    laid_out: (u32, u32),
) -> Result<EditorUiSurface, String> {
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::PRIMARY,
        flags: wgpu::InstanceFlags::from_build_config(),
        backend_options: wgpu::BackendOptions::default(),
    });
    let surface = instance
        .create_surface(wgpu::SurfaceTarget::Canvas(canvas))
        .map_err(|error| format!("UI canvas surface refused: {error}"))?;
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        })
        .await
        .map_err(|error| format!("no adapter for the UI canvas: {error}"))?;
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("pill editor egui device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            memory_hints: wgpu::MemoryHints::default(),
            trace: wgpu::Trace::default(),
        })
        .await
        .map_err(|error| format!("UI canvas device refused: {error}"))?;
    let capabilities = surface.get_capabilities(&adapter);
    let format = capabilities
        .formats
        .iter()
        .copied()
        .find(wgpu::TextureFormat::is_srgb)
        .unwrap_or(capabilities.formats[0]);
    let mut configuration = surface
        .get_default_config(&adapter, laid_out.0.max(1), laid_out.1.max(1))
        .ok_or("the UI canvas offers no surface configuration")?;
    configuration.format = format;
    // egui's output is premultiplied and the page stacks this canvas over the
    // scene one, so the canvas must composite with alpha. wgpu 25's web
    // backend supports it but its capability query reports only `Opaque`
    // (the WebGPU backend's `surface_capabilities` hard-codes that list), so
    // the mode is requested directly rather than picked from the list.
    configuration.alpha_mode = wgpu::CompositeAlphaMode::PreMultiplied;
    info!(
        target: telemetry_target::RENDERING,
        format = ?format,
        alpha_mode = ?configuration.alpha_mode,
        "UI canvas configured"
    );
    surface.configure(&device, &configuration);
    let renderer = egui_wgpu::Renderer::new(&device, format, None, 1, false);
    Ok(EditorUiSurface {
        surface,
        configuration,
        renderer,
        device,
        queue,
    })
}
