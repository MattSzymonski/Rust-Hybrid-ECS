//! The editor in the browser (Stage 30): the scene on one canvas, the
//! shell's UI on a canvas above it.
//!
//! # Responsibilities
//!
//! - [`run_editor`]: run a static project that links the editor shell, put
//!   the scene on one canvas and the shell's panels and overlays on a
//!   canvas pair above it, and drive the shell's plain-data frame
//!   (`EditorShellInput`/`EditorShellFrame`) the way the native executable
//!   does.
//! - [`editor_command`]: the scripted channel's entry point, the web
//!   counterpart of `PILL_EDITOR_TEST_COMMANDS` (Stage 30): a line per call,
//!   drained into the shell's input the next frame and logged for the
//!   headless suite to assert on.
//!
//! # Design
//!
//! The shell crate registers itself when the bundle calls its `register`
//! (its system runs inside every engine update); this module owns the
//! browser-only half: a second `wgpu` device and `UiRenderer` drawing the
//! frame's draw lists into the UI canvas - clear with `panels`, then
//! `overlay` and each popup on top, translated from popup-local pixels.
//! Free-standing windows and modals are native-only for now (the shell's
//! requests are left unanswered); the layout, preferences and keybinding
//! files keep their defaults, because a browser has no filesystem.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use pill_core::telemetry::telemetry_target;
use pill_core::{error, info, warn};
use pill_editor_api::{EditorShellFrame, EditorShellInput};
use pill_engine_core::input::{InputEvent, ScrollDelta};
use pill_input::winit_events::translate_window_event;
use pill_runtime::{FrameDriver, RenderingRuntime, Runtime, StaticProject};
use pill_ui::draw::Color;
use pill_ui_wgpu::UiRenderer;
use wasm_bindgen::prelude::wasm_bindgen;
use web_sys::HtmlCanvasElement;
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::platform::web::{EventLoopExtWebSys, WindowAttributesExtWebSys};
use winit::window::{Window, WindowId};

use crate::{find_canvas, focus_without_scrolling, physical_size, report_failure};

/// The atlas the UI renderer starts with; the shell's frame updates grow it.
const UI_ATLAS_SIZE: (u32, u32) = (512, 512);

/// Lines the page scripted through [`editor_command`], drained by the next
/// frame. Single-threaded like every wasm module.
thread_local! {
    static SCRIPTED_LINES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// The scripted channel's entry point (Stage 30): the web counterpart of
/// the native `PILL_EDITOR_TEST_COMMANDS` file. One line per call; the next
/// frame drains the queue into the shell's input.
#[wasm_bindgen]
pub fn editor_command(line: &str) {
    SCRIPTED_LINES.with(|lines| lines.borrow_mut().push(line.to_owned()));
}

/// Run `project` as an editor in the page: the scene on the canvas named
/// `scene_canvas_id`, the shell's UI on the canvas named `ui_canvas_id`.
///
/// Returns once the event loop is handed to the browser; the editor then
/// runs on the scene's animation frames. Every failure is reported to the
/// browser console.
pub fn run_editor(project: StaticProject, scene_canvas_id: &str, ui_canvas_id: &str) {
    console_error_panic_hook::set_once();
    if let Err(error) = pill_runtime::init_telemetry(None) {
        web_sys::console::error_1(&format!("[web] telemetry failed to start: {error}").into());
    }
    let runtime = match pill_runtime::setup(project) {
        Ok(runtime) => runtime,
        Err(error) => return report_failure("project setup", &error),
    };
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
        frames_per_second: 0.0,
        ui_frames: 0,
    });
}

/// The UI canvas's own device, surface and renderer.
struct EditorUiSurface {
    surface: wgpu::Surface<'static>,
    configuration: wgpu::SurfaceConfiguration,
    renderer: UiRenderer,
    device: wgpu::Device,
    queue: wgpu::Queue,
    /// Physical pixels per logical pixel of the UI canvas.
    scale: f32,
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
    /// The UI canvas's device and renderer, filled in when its own attach
    /// resolves.
    ui_surface: Rc<RefCell<Option<EditorUiSurface>>>,
    /// UI input events since the last frame, physical pixels.
    pending_events: Vec<InputEvent>,
    /// The scene's last frame rate, for the Statistics panel.
    frames_per_second: f32,
    /// UI frames drawn, for the console heartbeat.
    ui_frames: u64,
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
            .with_focusable(true);
        let scene_window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(error) => return report_failure("scene canvas window creation", &error),
        };
        self.scene_window = Some(Arc::clone(&scene_window));

        // Step 2: the UI canvas above it, with its own window for input; the
        // shell receives the window's events, not winit's focus logic.
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

        // Step 4: build the UI canvas's own device and renderer above it.
        let ui_surface = Rc::clone(&self.ui_surface);
        let ui_scale = ui_window.scale_factor() as f32;
        pill_core::platform::futures::spawn(async move {
            match attach_ui_surface(ui_canvas, ui_scale, ui_laid_out).await {
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
            for input in translate_window_event(&event) {
                self.pending_events.push(input);
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
    /// One editor frame: hand this frame's input to the shell, run the
    /// game's frame (the shell updates inside it), then draw the UI.
    fn frame(&mut self) {
        let Some(scene_window) = self.scene_window.clone() else {
            return;
        };
        // Step 1: this frame's UI input, in logical pixels, and the
        // scripted channel's queued lines.
        let scale_factor = self
            .ui_window
            .as_ref()
            .map_or(1.0, |window| window.scale_factor() as f32)
            .max(f32::EPSILON);
        let events: Vec<InputEvent> = std::mem::take(&mut self.pending_events)
            .into_iter()
            .map(|event| logical_event(event, scale_factor))
            .collect();
        let logical_size = self
            .ui_window
            .as_ref()
            .map(|window| {
                let size = window.inner_size();
                pill_core::math::Vector2f::new(size.width as f32, size.height as f32) / scale_factor
            })
            .unwrap_or(pill_core::math::Vector2f::new(1.0, 1.0));
        let lines: Vec<String> =
            SCRIPTED_LINES.with(|lines| std::mem::take(&mut *lines.borrow_mut()));
        let frames_per_second = self.frames_per_second;
        {
            let mut driver = self.driver.borrow_mut();
            let Some(driver) = driver.as_mut() else {
                return;
            };
            let world = driver.engine_mut().world_mut();
            if let Some(mut input) = world.get_resource_mut::<EditorShellInput>() {
                input.events.extend(events);
                input.logical_size = logical_size;
                input.scale_factor = scale_factor;
                input.editor_frames_per_second = frames_per_second;
                for line in lines {
                    apply_scripted_line(&mut input, line.trim());
                }
            }
        }
        // Step 2: the game's frame; the shell's system ran inside it. The
        // UI surface handle is cloned out first so the driver borrow and
        // the UI draw cannot conflict.
        let ui_surface = Rc::clone(&self.ui_surface);
        let mut ui_frames = self.ui_frames;
        {
            let mut driver = self.driver.borrow_mut();
            let Some(driver) = driver.as_mut() else {
                return;
            };
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
            // Step 3: the shell's frame, drawn into the UI canvas.
            draw_ui(&ui_surface, &mut ui_frames, driver.engine());
        }
        self.ui_frames = ui_frames;
        scene_window.request_redraw();
    }
}

/// Draw the shell's plain-data frame into the UI canvas: `panels` cleared
/// first, then `overlay` and each popup on top.
fn draw_ui(
    ui_surface: &Rc<RefCell<Option<EditorUiSurface>>>,
    ui_frames: &mut u64,
    engine: &pill_engine_core::Engine,
) {
    let mut ui_surface = ui_surface.borrow_mut();
    let Some(ui) = ui_surface.as_mut() else {
        return;
    };
    let world = engine.world();
    let Some(frame) = world.get_resource::<EditorShellFrame>() else {
        return;
    };
    if let Some(update) = &frame.atlas {
        ui.renderer
            .apply_atlas_update(&ui.device, &ui.queue, update);
    }
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
    let mut encoder = ui
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("pill editor ui"),
        });
    let size = (ui.configuration.width, ui.configuration.height);
    ui.renderer.render(
        &ui.device,
        &ui.queue,
        &mut encoder,
        &view,
        size,
        ui.scale,
        &frame.panels,
        Some(Color::rgba(0.0, 0.0, 0.0, 0.0)),
    );
    ui.renderer.render(
        &ui.device,
        &ui.queue,
        &mut encoder,
        &view,
        size,
        ui.scale,
        &frame.overlay,
        None,
    );
    for popup in &frame.popups {
        let mut draw = popup.draw.clone();
        draw.translate(pill_core::math::Vector2f::new(
            popup.rect.x as f32,
            popup.rect.y as f32,
        ));
        ui.renderer.render(
            &ui.device,
            &ui.queue,
            &mut encoder,
            &view,
            size,
            ui.scale,
            &draw,
            None,
        );
    }
    ui.queue.submit([encoder.finish()]);
    texture.present();
    *ui_frames += 1;
    if *ui_frames % 60 == 1 {
        // NOTE: asserted on by the headless web suite (Stage 30).
        info!(
            target: telemetry_target::ENGINE,
            frames = frame.frames,
            panels = frame.panels.vertices.len(),
            overlay = frame.overlay.vertices.len(),
            popups = frame.popups.len(),
            "[editor] web frame"
        );
    }
}

/// Translate one physical-pixel event into the shell's logical pixels.
fn logical_event(event: InputEvent, scale_factor: f32) -> InputEvent {
    match event {
        InputEvent::CursorMoved { position } => InputEvent::CursorMoved {
            position: position / scale_factor,
        },
        InputEvent::MouseMotion { delta } => InputEvent::MouseMotion {
            delta: delta / scale_factor,
        },
        InputEvent::MouseWheel {
            delta: ScrollDelta::Pixels(pixels),
        } => InputEvent::MouseWheel {
            delta: ScrollDelta::Pixels(pixels / scale_factor),
        },
        other => other,
    }
}

/// Apply one scripted line to the shell's input (Stage 30).
fn apply_scripted_line(input: &mut EditorShellInput, line: &str) {
    if line.is_empty() {
        return;
    }
    let (verb, argument) = match line.split_once(char::is_whitespace) {
        Some((verb, rest)) => (verb, rest.trim()),
        None => (line, ""),
    };
    match verb {
        "command" if !argument.is_empty() => {
            info!(target: telemetry_target::ENGINE, id = argument, "[editor] web command requested");
            input.requested_commands.push(argument.to_owned());
        }
        "tab" if !argument.is_empty() => {
            info!(target: telemetry_target::ENGINE, title = argument, "[editor] web tab requested");
            input.requested_tab = Some(argument.to_owned());
        }
        "activate" if !argument.is_empty() => {
            info!(target: telemetry_target::ENGINE, name = argument, "[editor] web activate requested");
            input.requested_widget = Some(argument.to_owned());
        }
        other => {
            warn!(target: telemetry_target::ENGINE, line, verb = other, "[editor] web command ignored");
        }
    }
}

/// Build the UI canvas's device, surface and renderer.
async fn attach_ui_surface(
    canvas: HtmlCanvasElement,
    scale: f32,
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
            label: Some("pill editor ui device"),
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
    configuration.alpha_mode = capabilities
        .alpha_modes
        .iter()
        .copied()
        .find(|mode| *mode == wgpu::CompositeAlphaMode::PreMultiplied)
        .unwrap_or(configuration.alpha_mode);
    surface.configure(&device, &configuration);
    let renderer = UiRenderer::new(&device, &queue, format, UI_ATLAS_SIZE);
    Ok(EditorUiSurface {
        surface,
        configuration,
        renderer,
        device,
        queue,
        scale,
    })
}
