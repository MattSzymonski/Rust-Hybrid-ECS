//! The editor's windows, GPU device and frame loop.
//!
//! # Responsibilities
//!
//! - Create the main window, the game and scene surfaces and the editor's
//!   own `wgpu` device with its window surface (D8: the executable owns its
//!   device; nothing is shared with the project's renderer, which presents
//!   into its own child surfaces).
//! - Draw the editor's UI every frame: `pill_ui` builds and paints the draw
//!   list, `pill_ui_wgpu` renders it into the window surface.
//! - Translate `winit` input into `pill_ui` events, scaled from physical to
//!   logical pixels, and route the surface windows' input: the game window's
//!   to the project, the scene window's to the editor camera (D29).
//!
//! # Design
//!
//! The loop mirrors `pill_standalone`'s (W-0005): `ControlFlow::Poll` with a
//! self-scheduling `request_redraw` after every presented frame. The panels
//! drawn here are temporary - the reloadable shell module builds them from
//! the next milestone on; this file owns the presentation, never the
//! content.
//!
//! The game surface is the renderer's primary: it is attached with
//! [`pill_host::setup_rendering`] exactly as a game frontend attaches one,
//! so it draws through the frame's chosen camera - the game's own. The
//! scene surface is attached as [`SCENE_SURFACE`] and bound to the editor
//! camera, which lives here as plain data and never enters the world.

// Standard library
use std::sync::Arc;

// External crates
use pill_core::math::Vector2f;
use pill_core::platform::futures::block_on;
use pill_core::platform::Instant;
use pill_core::telemetry::telemetry_target;
use pill_core::{info, warn};
use pill_editor_api::{EditorRect, EditorShellFrame, EditorShellInput};
use pill_editor_core::editor_state::EditorCommand;
use pill_editor_core::history::{CommandHistory, HistoryEntry};
use pill_engine_core::{ButtonState, InputEvent, KeyCode, Modifiers, MouseButton, ScrollDelta};
use pill_host::{HostConfig, RenderingHost};
use pill_input::winit_events::translate_window_event;
use pill_runtime::FrameDriver;
use pill_runtime::{CameraBinding, CameraComponent, SurfaceId, SurfaceOptions, TransformComponent};
use pill_ui::panel::ClipboardRequest;
use pill_ui::{paint, run_frame};
use pill_ui::{
    Atlas, AtlasUpdate, DrawList, PanelBuilder, Rect, Stylesheet, TextSystem, Theme, UiState,
};
use pill_ui_wgpu::UiRenderer;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::raw_window_handle::HasWindowHandle;
use winit::window::{Window, WindowId};

// Current crate
use crate::frontend::EditorError;
use crate::test_channel::{self, TestChannel, TestCommand};

// =============================================================================
// Constants
// =============================================================================

/// The editor UI's atlas size, in pixels; the sampler's one texture holds
/// every glyph and icon the UI draws.
const ATLAS_SIZE: (u32, u32) = (2048, 2048);

/// The window's initial size in logical pixels.
const INITIAL_WINDOW_SIZE: LogicalSize<f64> = LogicalSize::new(1280.0, 800.0);

/// The surfaces' initial size in logical pixels; the docking layout
/// replaces it before the first frame.
const INITIAL_SCENE_SIZE: LogicalSize<f64> = LogicalSize::new(640.0, 480.0);

/// How much of each new frame feeds the displayed FPS, the rest being the
/// running average.
const FPS_SMOOTHING: f32 = 0.1;

/// The scene view's surface id; the game view is the primary surface the
/// renderer is attached with.
const SCENE_SURFACE: SurfaceId = SurfaceId(1);

// =============================================================================
// run
// =============================================================================

/// Run the editor: the event loop, the window and every presented frame.
///
/// # Errors
///
/// Returns the first failure that ended the run - event loop, window, GPU
/// device - with a setup failure recorded before the loop reported at the
/// same boundary.
pub fn run() -> Result<(), EditorError> {
    // Step 1: the event loop, polling rather than waiting so a frame is
    // always one redraw request away.
    let event_loop =
        EventLoop::new().map_err(|source| EditorError::EventLoopCreation { source })?;
    event_loop.set_control_flow(ControlFlow::Poll);
    // Step 2: run until the last window closes.
    let mut application = EditorApplication::default();
    event_loop
        .run_app(&mut application)
        .map_err(|source| EditorError::EventLoopRun { source })?;
    // Step 3: a failure recorded during `resumed` is reported here, where
    // the loop has already exited and every window is gone.
    match application.failure.take() {
        Some(failure) => Err(failure),
        None => Ok(()),
    }
}

// =============================================================================
// EditorApplication
// =============================================================================

/// State the event loop carries between events.
#[derive(Default)]
struct EditorApplication {
    /// The editor window and its GPU state, once `resumed` created them.
    editor: Option<EditorWindow>,
    /// A failure recorded outside the event callbacks' return path;
    /// surfaced by [`run`] after the loop exits.
    failure: Option<EditorError>,
}

impl ApplicationHandler for EditorApplication {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.editor.is_some() {
            return;
        }
        match EditorWindow::open(event_loop) {
            Ok(editor) => {
                event_loop.set_control_flow(ControlFlow::Poll);
                // The first frame draws before the loop's first redraw
                // request, so the window is never shown empty.
                self.editor = Some(editor);
            }
            Err(error) => {
                self.failure = Some(error);
                event_loop.exit();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        // The game and scene surfaces are their own windows: the game
        // window's events are the game's input, the scene window's drive the
        // editor camera, the main window's the editor UI's.
        if editor.is_scene_window(window_id) {
            editor.scene_view_event(&event);
            return;
        }
        if editor.is_game_window(window_id) {
            editor.game_view_event(&event);
            return;
        }
        match event {
            WindowEvent::CloseRequested => {
                info!(
                    target: telemetry_target::ENGINE,
                    "[editor] window closed, shutting down"
                );
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                editor.resize(size.width, size.height);
            }
            WindowEvent::RedrawRequested => match editor.frame() {
                Ok(()) => {
                    if editor.quit_requested() {
                        info!(
                            target: telemetry_target::ENGINE,
                            "[editor] quitting on request"
                        );
                        event_loop.exit();
                    }
                }
                Err(error) => {
                    self.failure = Some(error);
                    event_loop.exit();
                }
            },
            // Everything else is input for the editor UI.
            event => editor.push_ui_input(&event),
        }
    }
}

// =============================================================================
// EditorWindow
// =============================================================================

/// The main window with the editor's own GPU device and UI renderer.
struct EditorWindow {
    /// The development host running the project and its extensions (the
    /// editor shell module joins them next). Declared first so it drops
    /// first: the renderer detaches before any window it draws into goes
    /// away, and the project's DLLs unload before the process state they
    /// touched.
    host: RenderingHost,
    /// The game surface the project's renderer presents into as its primary
    /// surface; it draws through the frame's chosen camera - the game's own
    /// (D29). Declared before the main window: the child is destroyed before
    /// its parent.
    game: SurfaceWindow,
    /// The scene surface, attached as [`SCENE_SURFACE`] and bound to the
    /// editor camera; the editor's navigation never reaches the game.
    scene: SurfaceWindow,
    /// Whether the scene surface was accepted by the project's renderer; a
    /// single-surface renderer refuses it and the Scene tab then stays
    /// empty.
    scene_attached: bool,
    /// The editor camera the scene surface draws through, as plain data:
    /// it is never an entity in the world (D29).
    camera: EditorCamera,
    /// Whether the renderer needs the camera re-bound this frame; set by
    /// camera changes only, so a still camera costs nothing.
    camera_dirty: bool,
    /// The winit window; redraws are requested through it.
    window: Arc<Window>,
    /// The editor's presentation surface.
    surface: wgpu::Surface<'static>,
    /// The editor's own device and queue (D8); the project's renderer has
    /// its own.
    device: wgpu::Device,
    queue: wgpu::Queue,
    /// The surface's configuration; `width`/`height` are physical pixels.
    configuration: wgpu::SurfaceConfiguration,
    /// The `pill_ui` renderer: one pipeline and the atlas texture.
    ui: UiRenderer,
    /// The UI's widget state, text engine, theme, stylesheet and atlas.
    state: UiState,
    text: TextSystem,
    theme: Theme,
    sheet: Stylesheet,
    atlas: Atlas,
    /// Input events since the last frame, in logical pixels, on their way
    /// to the shell module's input resource.
    pending_ui_events: Vec<InputEvent>,
    /// The scene rectangle the shell last asked for, in logical pixels:
    /// the placement survives a frame in which the shell had nothing to
    /// say (and is the fallback while no shell module is loaded).
    last_scene_rect: Option<EditorRect>,
    /// The game rectangle the shell last asked for, on the same terms.
    last_game_rect: Option<EditorRect>,
    /// A tab the scripted suite asked to select, by title; delivered to the
    /// shell with the next frame's input.
    requested_tab: Option<String>,
    /// Command ids the scripted suite asked to execute; delivered to the
    /// shell's dispatch with the next frame's input.
    requested_commands: Vec<String>,
    /// The devops suite's scripted input channel, when its environment
    /// variable named a file.
    test_channel: Option<TestChannel>,
    /// When the scripted run started, with `PILL_EDITOR_TEST_SECONDS` its
    /// graceful-quit deadline.
    test_started: Option<Instant>,
    test_seconds: Option<f32>,
    /// A graceful quit the loop should take at the end of this frame.
    quit_requested: bool,
    /// Signature of the component registry at the last check; a reload that
    /// changes it invalidates stale history entries.
    registry_epoch: u64,
    /// Frame pacing for the displayed FPS.
    last_frame: Instant,
    frames_per_second: f32,
    frame_count: u64,
}

impl EditorWindow {
    /// Create the window and its GPU state, and log the startup line the
    /// harness waits for.
    fn open(event_loop: &ActiveEventLoop) -> Result<EditorWindow, EditorError> {
        // Step 1: the window.
        let attributes = Window::default_attributes()
            .with_title("Pill Editor")
            .with_inner_size(INITIAL_WINDOW_SIZE)
            .with_min_inner_size(LogicalSize::new(640.0, 400.0));
        let window = Arc::new(
            event_loop
                .create_window(attributes)
                .map_err(|source| EditorError::WindowCreation { source })?,
        );
        // Step 2: the editor's own GPU device and window surface.
        let size = window.inner_size();
        let width = size.width.max(1);
        let height = size.height.max(1);
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            // The same choice the renderer module makes: the primary
            // backends, never a hidden GL helper window that outlives the
            // code that created it.
            backends: wgpu::Backends::from_env().unwrap_or(wgpu::Backends::PRIMARY),
            ..Default::default()
        });
        let surface = instance
            .create_surface(Arc::clone(&window))
            .map_err(|source| EditorError::SurfaceCreation { source })?;
        let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .map_err(|source| EditorError::AdapterRequest { source })?;
        let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("pill editor device"),
            ..Default::default()
        }))
        .map_err(|source| EditorError::DeviceRequest { source })?;
        // Step 3: pick the surface's format; the UI shader assumes the
        // target encodes linear light, so sRGB wins when offered.
        let capabilities = surface.get_capabilities(&adapter);
        let format = capabilities
            .formats
            .iter()
            .copied()
            .find(wgpu::TextureFormat::is_srgb)
            .unwrap_or(wgpu::TextureFormat::Bgra8UnormSrgb);
        let present_mode = if capabilities
            .present_modes
            .contains(&wgpu::PresentMode::Fifo)
        {
            wgpu::PresentMode::Fifo
        } else {
            capabilities.present_modes[0]
        };
        let alpha_mode = capabilities.alpha_modes[0];
        let configuration = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width,
            height,
            present_mode,
            alpha_mode,
            view_formats: Vec::new(),
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &configuration);
        // Step 4: the UI's renderer and state.
        let ui = UiRenderer::new(&device, &queue, format, ATLAS_SIZE);
        // Step 5: the two view surfaces and the project host. The game
        // window is the renderer's primary surface, attached exactly as a
        // game frontend attaches one; the scene window is a second surface
        // bound to the editor camera (D29).
        let game = SurfaceWindow::open(event_loop, &window, "Pill Editor Game", "game")?;
        let scene = SurfaceWindow::open(event_loop, &window, "Pill Editor Scene", "scene")?;
        let (game_width, game_height) = game.size();
        let (scene_width, scene_height) = scene.size();
        let config =
            HostConfig::from_environment_for_editor().map_err(pill_core::error::HostError::from)?;
        let mut host =
            pill_host::setup_rendering(config, Arc::clone(&game.window), game_width, game_height)?;
        // The editor is an editor: asset metadata is maintained.
        host.host_mut().set_ensure_asset_metadata(true);
        info!(
            target: telemetry_target::ENGINE,
            detached = game.detached,
            "[editor] game surface attached"
        );
        // The scene surface is the renderer's choice: one that draws a
        // single surface refuses it, and the Scene tab then stays empty
        // while the editor carries on.
        let scene_attached = match host.attach_surface(
            SCENE_SURFACE,
            Arc::clone(&scene.window),
            scene_width,
            scene_height,
            SurfaceOptions::default(),
        ) {
            Ok(()) => {
                info!(
                    target: telemetry_target::ENGINE,
                    detached = scene.detached,
                    "[editor] scene surface attached"
                );
                true
            }
            Err(error) => {
                warn!(
                    target: telemetry_target::ENGINE,
                    error = %error,
                    "[editor] scene surface refused; the scene view stays empty"
                );
                false
            }
        };
        // The editor camera is plain data bound to the scene surface; the
        // game surface keeps the frame's own choice among the world's
        // cameras.
        let camera = EditorCamera::default();
        host.set_surface_camera(SCENE_SURFACE, Some(camera.binding()));
        info!(
            target: telemetry_target::ENGINE,
            width, height, "[editor] window created"
        );
        Ok(EditorWindow {
            host,
            game,
            scene,
            scene_attached,
            camera,
            camera_dirty: false,
            window,
            surface,
            device,
            queue,
            configuration,
            ui,
            state: UiState::new(),
            text: TextSystem::new(),
            theme: Theme::default(),
            sheet: Stylesheet::parse(pill_ui::DEFAULT_SHEET),
            atlas: Atlas::new(ATLAS_SIZE.0, ATLAS_SIZE.1),
            pending_ui_events: Vec::new(),
            last_scene_rect: None,
            last_game_rect: None,
            requested_tab: None,
            requested_commands: Vec::new(),
            test_channel: TestChannel::from_environment(),
            test_started: Some(Instant::now()),
            test_seconds: test_channel::seconds_from_environment(),
            quit_requested: false,
            registry_epoch: 0,
            last_frame: Instant::now(),
            frames_per_second: 0.0,
            frame_count: 0,
        })
    }

    /// The window was resized: track the new physical size and reconfigure.
    fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.configuration.width = width;
        self.configuration.height = height;
        self.surface.configure(&self.device, &self.configuration);
        // A resize redraws immediately; the overlay and the scene surfaces
        // follow in later milestones.
        self.window.request_redraw();
    }

    /// Queue one `winit` event's engine form into the UI input, in logical
    /// pixels.
    fn push_ui_input(&mut self, event: &WindowEvent) {
        let scale_factor = self.window.scale_factor() as f32;
        for mut event in translate_window_event(event) {
            // The UI works in logical pixels; winit reports physical ones.
            match &mut event {
                InputEvent::CursorMoved { position }
                | InputEvent::MouseMotion { delta: position } => {
                    position.x /= scale_factor;
                    position.y /= scale_factor;
                }
                InputEvent::MouseWheel {
                    delta: ScrollDelta::Pixels(pixels),
                } => {
                    pixels.x /= scale_factor;
                    pixels.y /= scale_factor;
                }
                _ => {}
            }
            // The shell module consumes these through its input resource;
            // the executable's own UI state takes them for the fallback.
            self.state.input.apply(&event);
            self.pending_ui_events.push(event);
        }
    }

    /// The executable's own minimal UI, used while the shell module is not
    /// loaded (or failed): the temporary Statistics panel over the window
    /// fill. Also the base the safe mode of a later milestone builds on.
    fn fallback_ui_frame(&mut self, available: Vector2f) -> (DrawList, Vec<ClipboardRequest>) {
        self.state.input.begin_frame();
        let frames_per_second = self.frames_per_second;
        let frame_count = self.frame_count;
        let (output, layout) = run_frame(
            &mut self.state,
            &mut self.text,
            &self.theme,
            &mut self.sheet,
            available,
            |ui| statistics_panel(ui, frames_per_second, frame_count),
        );
        let mut list = DrawList::new();
        list.push_fill(
            Rect::from_min_size(Vector2f::ZERO, available),
            self.theme.window,
        );
        paint(
            &output.tree,
            &layout,
            &self.theme,
            &self.state,
            &mut self.text,
            &mut self.atlas,
            &mut list,
        );
        if let Some(update) = self.atlas.take_update() {
            self.ui.apply_atlas_update(&self.queue, &update);
        }
        (list, output.clipboard)
    }

    /// Whether a window id is the scene surface's.
    fn is_scene_window(&self, id: WindowId) -> bool {
        id == self.scene.window.id()
    }

    /// Whether a window id is the game surface's.
    fn is_game_window(&self, id: WindowId) -> bool {
        id == self.game.window.id()
    }

    /// Whether a graceful quit was requested (the test channel, or the
    /// scripted seconds deadline); the event handler exits the loop after
    /// the frame that raised it.
    fn quit_requested(&self) -> bool {
        self.quit_requested
    }

    /// Execute whatever the scripted suite appended since the last frame,
    /// and enforce `PILL_EDITOR_TEST_SECONDS`.
    fn poll_test_channel(&mut self) {
        if let Some(channel) = &mut self.test_channel {
            for command in channel.poll() {
                match command {
                    TestCommand::Click { x, y } => self.inject_click(x, y),
                    TestCommand::Key { key, target } => self.inject_key(&key, target.as_deref()),
                    TestCommand::Tab { title } => {
                        info!(
                            target: telemetry_target::ENGINE,
                            title = %title,
                            "[editor] test channel: tab request"
                        );
                        self.requested_tab = Some(title);
                    }
                    TestCommand::Command { id } => {
                        info!(
                            target: telemetry_target::ENGINE,
                            id = %id,
                            "[editor] test channel: command request"
                        );
                        self.requested_commands.push(id);
                    }
                    TestCommand::Chord { text } => self.inject_chord(&text),
                    TestCommand::Edit(edit) => self.apply_edit(edit),
                    TestCommand::Dump(query) => self.dump_world(query),
                    TestCommand::Quit => {
                        info!(
                            target: telemetry_target::ENGINE,
                            "[editor] test channel: quit requested"
                        );
                        self.quit_requested = true;
                    }
                    TestCommand::Unknown { line } => {
                        warn!(
                            target: telemetry_target::ENGINE,
                            line = %line,
                            "[editor] test channel: unknown command"
                        );
                    }
                }
            }
        }
        if let (Some(started), Some(seconds)) = (self.test_started, self.test_seconds) {
            if started.elapsed().as_secs_f32() >= seconds {
                info!(
                    target: telemetry_target::ENGINE,
                    seconds,
                    "[editor] test seconds elapsed: quit requested"
                );
                self.quit_requested = true;
            }
        }
    }

    /// Queue one scripted edit through the recorded command path.
    ///
    /// The suite's edits take exactly the route a panel's edit takes: an
    /// `EditorCommand` batch queued for the frame boundary with recording on,
    /// so undo and redo cover scripted edits like any other.
    fn apply_edit(&mut self, edit: test_channel::EditCommand) {
        let engine = self.host.host_mut().engine_mut();
        let world = engine.world();
        let label: String;
        let mut coalesce = None;
        let command = match &edit {
            test_channel::EditCommand::Create { component, fields } => {
                let Some(component) = resolve_component(world, component) else {
                    warn!(
                        target: telemetry_target::ENGINE,
                        component = component.as_str(),
                        "[editor] test edit: component not resolvable"
                    );
                    return;
                };
                let mut seeds = Vec::new();
                let mut resolved = Vec::new();
                for (field, value) in fields {
                    match parse_field_value(value) {
                        Ok(value) => resolved.push((field.clone(), value)),
                        Err(message) => {
                            warn!(target: telemetry_target::ENGINE, message, "[editor] test edit: bad value");
                            return;
                        }
                    }
                }
                seeds.push(pill_editor_core::editor_state::ComponentSeed {
                    type_name: component.clone(),
                    fields: resolved,
                });
                label = format!("create {}", short_component_name(&component));
                EditorCommand::CreateEntity { components: seeds }
            }
            test_channel::EditCommand::Set {
                entity,
                component,
                field,
                value,
                coalesce: key,
            } => {
                let (Some(entity), Some(component), Ok(value)) = (
                    resolve_entity(world, entity),
                    resolve_component(world, component),
                    parse_field_value(value),
                ) else {
                    warn!(
                        target: telemetry_target::ENGINE,
                        "[editor] test edit: set target not resolvable"
                    );
                    return;
                };
                label = format!("set {}.{}", short_component_name(&component), field);
                coalesce = key.clone();
                EditorCommand::SetField {
                    entity,
                    component,
                    field: field.clone(),
                    value,
                }
            }
            test_channel::EditCommand::Array {
                entity,
                component,
                field,
                index,
                value,
                coalesce: key,
            } => {
                let (Some(entity), Some(component), Ok(value)) = (
                    resolve_entity(world, entity),
                    resolve_component(world, component),
                    parse_field_value(value),
                ) else {
                    warn!(
                        target: telemetry_target::ENGINE,
                        "[editor] test edit: array target not resolvable"
                    );
                    return;
                };
                label = format!(
                    "set {}[{}].{}",
                    short_component_name(&component),
                    index,
                    field
                );
                coalesce = key.clone();
                EditorCommand::SetArrayElement {
                    entity,
                    component,
                    field: field.clone(),
                    index: *index,
                    value,
                }
            }
            test_channel::EditCommand::Add { entity, component } => {
                let (Some(entity), Some(component)) = (
                    resolve_entity(world, entity),
                    resolve_component(world, component),
                ) else {
                    warn!(
                        target: telemetry_target::ENGINE,
                        "[editor] test edit: add target not resolvable"
                    );
                    return;
                };
                label = format!("add {}", short_component_name(&component));
                EditorCommand::AddComponent { entity, component }
            }
            test_channel::EditCommand::Remove { entity, component } => {
                let (Some(entity), Some(component)) = (
                    resolve_entity(world, entity),
                    resolve_component(world, component),
                ) else {
                    warn!(
                        target: telemetry_target::ENGINE,
                        "[editor] test edit: remove target not resolvable"
                    );
                    return;
                };
                label = format!("remove {}", short_component_name(&component));
                EditorCommand::RemoveComponent { entity, component }
            }
            test_channel::EditCommand::Destroy { entity } => {
                let Some(entity) = resolve_entity(world, entity) else {
                    warn!(
                        target: telemetry_target::ENGINE,
                        "[editor] test edit: destroy target not resolvable"
                    );
                    return;
                };
                label = "destroy".to_string();
                EditorCommand::DestroyEntity { entity }
            }
        };
        let world = engine.world_mut();
        if let Some(pending) =
            world.get_resource_mut::<pill_editor_core::commands::PendingEditorCommands>()
        {
            pending.push_recorded(vec![command], label, coalesce);
        }
    }

    /// Answer one scripted world query on the log, for the suite to assert.
    fn dump_world(&mut self, query: test_channel::DumpQuery) {
        let engine = self.host.host_mut().engine_mut();
        let world = engine.world();
        match query {
            test_channel::DumpQuery::Components => {
                // Deduplicated by name: the registry remembers one entry per
                // reload generation of a type.
                let mut seen: Vec<String> = Vec::new();
                for (name, id) in world.registered_components() {
                    if seen.contains(&name) {
                        continue;
                    }
                    seen.push(name.clone());
                    let fields = world
                        .component_field_layout(id)
                        .map(|layout| {
                            layout
                                .iter()
                                .map(|field| field.name.to_string())
                                .collect::<Vec<_>>()
                                .join(",")
                        })
                        .unwrap_or_default();
                    info!(
                        target: telemetry_target::ENGINE,
                        name = name.as_str(),
                        fields = fields.as_str(),
                        "[test] component"
                    );
                }
            }
            test_channel::DumpQuery::Entities => {
                // Every live entity with its component names; the suite
                // diffs these lists to learn a created handle.
                let rows = world.entity_rows();
                info!(
                    target: telemetry_target::ENGINE,
                    count = rows.len(),
                    "[test] entities"
                );
                for row in rows.iter().take(256) {
                    info!(
                        target: telemetry_target::ENGINE,
                        entity = %row.entity,
                        components = row.components.join(",").as_str(),
                        "[test] entity"
                    );
                }
            }
            test_channel::DumpQuery::Entity { entity } => match resolve_entity(world, &entity) {
                Some(resolved) => info!(
                    target: telemetry_target::ENGINE,
                    entity = %resolved,
                    "[test] entity alive"
                ),
                None => info!(
                    target: telemetry_target::ENGINE,
                    entity = entity.as_str(),
                    "[test] entity gone"
                ),
            },
            test_channel::DumpQuery::Field {
                entity,
                component,
                field,
            } => {
                let resolved = resolve_entity(world, &entity)
                    .zip(resolve_component(world, &component).map(|c| c));
                match resolved {
                    Some((resolved, component)) => {
                        match world.read_component_field(resolved, &component, &field) {
                            Ok(value) => info!(
                                target: telemetry_target::ENGINE,
                                entity = %resolved,
                                field = field.as_str(),
                                value = ?value,
                                "[test] field"
                            ),
                            Err(error) => info!(
                                target: telemetry_target::ENGINE,
                                entity = %resolved,
                                field = field.as_str(),
                                error = %error,
                                "[test] field unreadable"
                            ),
                        }
                    }
                    None => info!(
                        target: telemetry_target::ENGINE,
                        entity = entity.as_str(),
                        field = field.as_str(),
                        "[test] field unreadable"
                    ),
                }
            }
        }
    }

    /// A scripted click at a logical client point: delivered to the game,
    /// the scene view or the UI by the same rectangle rules real input
    /// follows, with the decision logged for the suite to assert on. A
    /// click in the scene view never reaches the game (D29).
    fn inject_click(&mut self, x: f32, y: f32) {
        let in_rect = |rect: Option<EditorRect>| {
            rect.is_some_and(|rect| {
                x >= rect.x && x <= rect.x + rect.width && y >= rect.y && y <= rect.y + rect.height
            })
        };
        let game_target = in_rect(self.last_game_rect);
        let scene_target = !game_target && in_rect(self.last_scene_rect);
        info!(
            target: telemetry_target::ENGINE,
            x,
            y,
            target = if game_target {
                "game"
            } else if scene_target {
                "scene"
            } else {
                "ui"
            },
            "[editor] test click"
        );
        // A press and a release in the same frame: widgets activate on the
        // release edge (the next frame's build), games see the pair.
        let presses = |position: Vector2f| {
            [
                InputEvent::CursorMoved { position },
                InputEvent::MouseButton {
                    button: MouseButton::Left,
                    state: ButtonState::Pressed,
                },
                InputEvent::MouseButton {
                    button: MouseButton::Left,
                    state: ButtonState::Released,
                },
            ]
        };
        if game_target {
            let scale_factor = self.window.scale_factor() as f32;
            for event in presses(Vector2f::new(x * scale_factor, y * scale_factor)) {
                self.host.push_input(event);
            }
        } else if scene_target {
            // Selection in the scene view arrives with Stage 20; the click
            // is accounted for and goes no further.
            info!(
                target: telemetry_target::ENGINE,
                x,
                y,
                "[editor] scene view click (selection arrives with Stage 20)"
            );
        } else {
            for event in presses(Vector2f::new(x, y)) {
                self.state.input.apply(&event);
                self.pending_ui_events.push(event);
            }
        }
    }

    /// A scripted key: press and release, to the UI, the scene view's
    /// editor camera or the game, by the command's target (default: the
    /// UI). Names are the lowercase words from the command file.
    fn inject_key(&mut self, key: &str, target: Option<&str>) {
        let key_code = match key {
            "escape" => KeyCode::Escape,
            "space" => KeyCode::Space,
            "w" => KeyCode::KeyW,
            "a" => KeyCode::KeyA,
            other => {
                warn!(
                    target: telemetry_target::ENGINE,
                    key = %other,
                    "[editor] test channel: unknown key"
                );
                return;
            }
        };
        let target = target.unwrap_or("ui");
        info!(
            target: telemetry_target::ENGINE,
            key = %key,
            target,
            "[editor] test key"
        );
        match target {
            // The scene view's keys drive the editor camera; they never
            // reach the game.
            "scene" => match key_code {
                KeyCode::KeyW => self.camera.fly(1.0),
                KeyCode::KeyA => self.camera.strafe(-1.0),
                _ => {}
            },
            // The game's keys go to the project exactly as the game
            // window's real keys do.
            "game" => {
                for state in [ButtonState::Pressed, ButtonState::Released] {
                    self.host.push_input(InputEvent::Key {
                        key: key_code,
                        state,
                    });
                }
            }
            // Everything else is the editor UI's, as before.
            _ => {
                for state in [ButtonState::Pressed, ButtonState::Released] {
                    let event = InputEvent::Key {
                        key: key_code,
                        state,
                    };
                    self.state.input.apply(&event);
                    self.pending_ui_events.push(event);
                }
            }
        }
        if self.camera.take_dirty() {
            self.camera_dirty = true;
        }
    }

    /// A scripted chord: modifiers plus a key, pressed and released, through
    /// the same UI input stream real keys use, so the shell's shortcut
    /// dispatch resolves it exactly as a physical key would.
    fn inject_chord(&mut self, text: &str) {
        let chord = match text.parse::<pill_editor_core::commands::Chord>() {
            Ok(chord) => chord,
            Err(error) => {
                warn!(
                    target: telemetry_target::ENGINE,
                    text = %text,
                    error = %error,
                    "[editor] test channel: invalid chord"
                );
                return;
            }
        };
        let Some(key) = chord.key.engine_key() else {
            warn!(
                target: telemetry_target::ENGINE,
                text = %text,
                "[editor] test channel: chord key unknown to the engine"
            );
            return;
        };
        info!(
            target: telemetry_target::ENGINE,
            chord = %text,
            target = "ui",
            "[editor] test chord"
        );
        let modifiers = Modifiers {
            shift: chord.shift,
            control: chord.ctrl,
            alt: chord.alt,
            super_key: false,
        };
        for event in [
            InputEvent::ModifiersChanged { modifiers },
            InputEvent::Key {
                key,
                state: ButtonState::Pressed,
            },
            InputEvent::Key {
                key,
                state: ButtonState::Released,
            },
            InputEvent::ModifiersChanged {
                modifiers: Modifiers::default(),
            },
        ] {
            self.state.input.apply(&event);
            self.pending_ui_events.push(event);
        }
    }

    /// Route one game-window event: input to the game, a resize marker to
    /// the placement logic. The operating system delivers keys to this
    /// window only while it holds focus, which is the explicit focus rule
    /// the plan wants (keys reach the game only with the game view focused,
    /// D29).
    fn game_view_event(&mut self, event: &WindowEvent) {
        if matches!(event, WindowEvent::Resized(_)) {
            self.game.placed = None;
        }
        for event in translate_window_event(event) {
            self.host.push_input(event);
        }
    }

    /// Route one scene-window event into the editor camera (D29): the scene
    /// view never forwards input to the game. Orbit on right drag, pan on
    /// middle drag, dolly on the wheel, and WASD moves the focus point.
    fn scene_view_event(&mut self, event: &WindowEvent) {
        if matches!(event, WindowEvent::Resized(_)) {
            self.scene.placed = None;
        }
        match event {
            WindowEvent::CursorMoved { position, .. } => {
                if let Some((last_x, last_y)) = self.scene.last_cursor {
                    let delta_x = position.x as f32 - last_x;
                    let delta_y = position.y as f32 - last_y;
                    if self.scene.orbiting {
                        self.camera.orbit(delta_x, delta_y);
                    } else if self.scene.panning {
                        self.camera.pan(delta_x, delta_y);
                    }
                }
                self.scene.last_cursor = Some((position.x as f32, position.y as f32));
            }
            WindowEvent::MouseInput { state, button, .. } => match button {
                winit::event::MouseButton::Right => {
                    self.scene.orbiting = *state == winit::event::ElementState::Pressed;
                }
                winit::event::MouseButton::Middle => {
                    self.scene.panning = *state == winit::event::ElementState::Pressed;
                }
                _ => {}
            },
            WindowEvent::MouseWheel { delta, .. } => {
                let amount = match delta {
                    winit::event::MouseScrollDelta::LineDelta(_, lines) => *lines,
                    winit::event::MouseScrollDelta::PixelDelta(position) => position.y as f32,
                };
                self.camera.dolly(amount);
            }
            WindowEvent::KeyboardInput { event, .. }
                if event.state == winit::event::ElementState::Pressed =>
            {
                let key = match event.physical_key {
                    winit::keyboard::PhysicalKey::Code(code) => code,
                    _ => return,
                };
                let direction = match key {
                    winit::keyboard::KeyCode::KeyW => Some(1.0),
                    winit::keyboard::KeyCode::KeyS => Some(-1.0),
                    _ => None,
                };
                if let Some(direction) = direction {
                    self.camera.fly(direction);
                }
                match key {
                    winit::keyboard::KeyCode::KeyA => self.camera.strafe(-1.0),
                    winit::keyboard::KeyCode::KeyD => self.camera.strafe(1.0),
                    _ => {}
                }
            }
            _ => {}
        }
        // Any camera change is published before the next frame renders
        // through it.
        if self.camera.take_dirty() {
            self.camera_dirty = true;
        }
    }

    /// Draw and present one frame of the editor UI.
    fn frame(&mut self) -> Result<(), EditorError> {
        // Step 1: pace the frame and update the displayed FPS.
        let seconds = self.last_frame.elapsed().as_secs_f32();
        self.last_frame = Instant::now();
        if seconds > 0.0 {
            let instant = 1.0 / seconds;
            self.frames_per_second = if self.frames_per_second == 0.0 {
                instant
            } else {
                self.frames_per_second * (1.0 - FPS_SMOOTHING) + instant * FPS_SMOOTHING
            };
        }
        self.frame_count += 1;
        // Step 2: the scripted suite's commands, and its seconds deadline.
        self.poll_test_channel();
        // Step 3: this frame's size, and the input queued since the last
        // frame, delivered to the shell module's input resource; the
        // executable's own state keeps its copy for the fallback.
        let scale_factor = self.window.scale_factor() as f32;
        let available = Vector2f::new(
            self.configuration.width as f32 / scale_factor,
            self.configuration.height as f32 / scale_factor,
        );
        {
            let systems_paused = self.host.host_mut().engine_mut().systems_paused();
            let world = self.host.host_mut().engine_mut().world_mut();
            if let Some(input) = world.get_resource_mut::<EditorShellInput>() {
                input.events.append(&mut self.pending_ui_events);
                input.logical_size = available;
                input.scale_factor = scale_factor;
                input.editor_frames_per_second = self.frames_per_second;
                input.requested_tab = self.requested_tab.take();
                input
                    .requested_commands
                    .append(&mut self.requested_commands);
                input.systems_paused = systems_paused;
            } else {
                // No shell module: the fallback already consumed the events.
                self.pending_ui_events.clear();
            }
        }
        // Step 3a: a changed editor camera is published before the frame
        // renders through it, so a gesture shows up in the very next frame.
        if self.camera_dirty {
            self.camera_dirty = false;
            self.host
                .set_surface_camera(SCENE_SURFACE, Some(self.camera.binding()));
        }
        // Step 3: run the project's frame. The shell's system runs inside
        // it, between the input just delivered and the output read next.
        if let Some(report) = self.host.run_frame().map_err(EditorError::Renderer)? {
            info!(
                target: telemetry_target::ENGINE,
                fps = report.fps,
                entities = report.entity_count,
                "[editor] scene frame report"
            );
        }
        // Step 4: take the shell's output back out of the world. The lists
        // are moved out and the shell rebuilds them next frame, so no large
        // value is ever copied or held borrows across the presentation.
        let shell_output = {
            let world = self.host.host_mut().engine_mut().world_mut();
            world
                .get_resource_mut::<EditorShellFrame>()
                .map(|frame| ShellOutput {
                    panels: std::mem::take(&mut frame.panels),
                    atlas: frame.atlas.take(),
                    clipboard: std::mem::take(&mut frame.clipboard),
                    scene_rect: frame.scene_rect,
                    game_rect: frame.game_rect,
                })
        };
        let output = match shell_output {
            Some(output) => output,
            None => {
                let (panels, clipboard) = self.fallback_ui_frame(available);
                ShellOutput {
                    panels,
                    atlas: None,
                    clipboard,
                    scene_rect: self.last_scene_rect,
                    game_rect: self.last_game_rect,
                }
            }
        };
        // Step 4b: apply the engine mutations the shell's command handlers
        // queued. This is the frame boundary the `EditorCommand` contract
        // names; a failed command never aborts the rest of the batch.
        {
            // Step 4b-1: a reload that changed the component registry or a
            // layout invalidates history entries that name what is gone.
            {
                let engine = self.host.host_mut().engine_mut();
                let epoch = component_registry_epoch(engine);
                if epoch != self.registry_epoch {
                    self.registry_epoch = epoch;
                    let world = engine.world_mut();
                    // Snapshot what exists now; the validity walk then needs
                    // no engine borrow of its own.
                    let mut layouts: Vec<(String, Vec<String>)> = Vec::new();
                    for (name, id) in world.registered_components() {
                        let fields = world
                            .component_field_layout(id)
                            .map(|layout| {
                                layout.iter().map(|field| field.name.to_string()).collect()
                            })
                            .unwrap_or_default();
                        layouts.push((name, fields));
                    }
                    if let Some(history) = world.get_resource_mut::<CommandHistory>() {
                        let dropped =
                            history.drop_stale(|identifier| match identifier.split_once('.') {
                                Some((component, field)) => layouts.iter().any(|(name, fields)| {
                                    name == component && fields.iter().any(|known| known == field)
                                }),
                                None => layouts.iter().any(|(name, _)| name == identifier),
                            });
                        if dropped > 0 {
                            info!(
                                target: telemetry_target::ENGINE,
                                dropped,
                                "[editor] history dropped stale entries"
                            );
                        }
                    }
                }
            }
            let batches = {
                let world = self.host.host_mut().engine_mut().world_mut();
                world
                    .get_resource_mut::<pill_editor_core::commands::PendingEditorCommands>()
                    .map(|pending| std::mem::take(&mut pending.batches))
                    .unwrap_or_default()
            };
            let now_millis = self
                .test_started
                .map(|start| start.elapsed().as_millis() as u64)
                .unwrap_or(0);
            for batch in batches {
                let engine = self.host.host_mut().engine_mut();
                let recorded = EditorCommand::apply_recorded(engine, &batch.commands);
                info!(
                    target: telemetry_target::ENGINE,
                    count = batch.commands.len(),
                    "[editor] engine commands applied"
                );
                for (index, message) in recorded.failures {
                    warn!(
                        target: telemetry_target::ENGINE,
                        index,
                        message = %message,
                        "[editor] command failed"
                    );
                }
                // Step 4b-2: a restore assigned fresh handles; the history
                // follows them before anything else is recorded, so a later
                // undo cannot target the dead handle.
                let world = self.host.host_mut().engine_mut().world_mut();
                let Some(history) = world.get_resource_mut::<CommandHistory>() else {
                    continue;
                };
                for (previous, assigned) in &recorded.remaps {
                    history.remap_entity(*previous, *assigned);
                    info!(
                        target: telemetry_target::ENGINE,
                        previous = %previous,
                        assigned = %assigned,
                        "[editor] history remap"
                    );
                }
                // Step 4b-3: an edit batch becomes one undo step, unless it
                // failed or could not be inverted in full - a step that could
                // not be undone would be worse than no step.
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
                    let outcome =
                        history.push(entry, now_millis, CommandHistory::DEFAULT_COALESCE_WINDOW);
                    info!(
                        target: telemetry_target::ENGINE,
                        label = batch.label.as_str(),
                        steps = outcome.steps,
                        coalesced = outcome.coalesced,
                        "[editor] history push"
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
            }
        }
        // Step 5: both view surfaces go where the shell's layout put them.
        // The shell reports a rectangle only for the selected view tab, so
        // the other view hides instead of sitting under it; a shell-less
        // frame carries the previous frame's rectangles through the fallback
        // output, which keeps the placement stable.
        self.last_scene_rect = output.scene_rect;
        self.last_game_rect = output.game_rect;
        let scene_rect = if self.scene_attached {
            self.last_scene_rect
        } else {
            None
        };
        place_view(&mut self.scene, scene_rect, scale_factor, "scene");
        place_view(&mut self.game, self.last_game_rect, scale_factor, "game");
        // Step 6: the atlas additions of whichever UI built this frame.
        if let Some(update) = output.atlas {
            self.ui.apply_atlas_update(&self.queue, &update);
        }
        // Step 7: clipboard requests wait for the platform layer's Win32
        // implementation (a later milestone of this stage); report them
        // meanwhile so nothing is silently dropped.
        for request in &output.clipboard {
            info!(
                target: telemetry_target::ENGINE,
                request = ?request,
                "[editor] clipboard request pending platform support"
            );
        }
        // Step 8: acquire the surface image, treating transient failures as
        // a skipped frame.
        let frame = match self.surface.get_current_texture() {
            Ok(frame) => frame,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                self.surface.configure(&self.device, &self.configuration);
                return Ok(());
            }
            Err(wgpu::SurfaceError::OutOfMemory) => return Err(EditorError::OutOfMemory),
            Err(error) => {
                warn!(
                    target: telemetry_target::ENGINE,
                    error = %error,
                    "[editor] skipping frame: the surface was not acquirable"
                );
                return Ok(());
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("editor frame"),
            });
        self.ui.render(
            &self.device,
            &self.queue,
            &mut encoder,
            &view,
            (self.configuration.width, self.configuration.height),
            scale_factor,
            &output.panels,
            Some(self.theme.window),
        );
        self.queue.submit([encoder.finish()]);
        frame.present();
        // Step 9: the loop is the pacing; the next frame starts when this
        // one finished.
        self.window.request_redraw();
        Ok(())
    }
}

// =============================================================================
// ShellOutput
// =============================================================================

/// One frame of editor UI, from the shell module or from the fallback: the
/// draw list to present and what to do around it.
struct ShellOutput {
    /// The UI under the scene layer.
    panels: DrawList,
    /// The frame's atlas additions, for the executable's renderer.
    atlas: Option<AtlasUpdate>,
    /// Clipboard requests to fulfil (platform support pending).
    clipboard: Vec<ClipboardRequest>,
    /// Where the scene surface goes, in logical pixels.
    scene_rect: Option<EditorRect>,
    /// Where the game surface goes, in logical pixels.
    game_rect: Option<EditorRect>,
}

/// Show and place a view surface at its layout rectangle, in logical pixels,
/// or hide it while its tab is not selected.
fn place_view(
    surface: &mut SurfaceWindow,
    rect: Option<EditorRect>,
    scale_factor: f32,
    view: &str,
) {
    let Some(rect) = rect else {
        if surface.set_shown(false) {
            info!(
                target: telemetry_target::ENGINE,
                view,
                "[editor] view hidden"
            );
        }
        return;
    };
    if surface.set_shown(true) {
        info!(
            target: telemetry_target::ENGINE,
            view,
            x = rect.x,
            y = rect.y,
            width = rect.width,
            height = rect.height,
            "[editor] view shown"
        );
    }
    let x = (rect.x * scale_factor).round() as i32;
    let y = (rect.y * scale_factor).round() as i32;
    let width = (rect.width * scale_factor).round().max(1.0) as u32;
    let height = (rect.height * scale_factor).round().max(1.0) as u32;
    surface.place(x, y, width, height);
}

// =============================================================================
// SurfaceWindow
// =============================================================================

/// One view surface's window: its placement in physical pixels, its input
/// gesture state, and whether it is currently shown.
struct SurfaceWindow {
    /// The child window - or, when the platform refused one, the detached
    /// top-level fallback - the project's renderer presents into.
    window: Arc<Window>,
    /// Whether this is the detached fallback rather than a child window.
    detached: bool,
    /// The last placed rectangle in physical pixels, so redundant moves
    /// and resizes are skipped.
    placed: Option<(i32, i32, u32, u32)>,
    /// Whether the window is currently shown; a view whose tab is not
    /// selected is hidden rather than left covering the other one.
    visible: bool,
    /// Whether the scene navigation is orbiting (right drag held).
    orbiting: bool,
    /// Whether the scene navigation is panning (middle drag held).
    panning: bool,
    /// The cursor position of the previous motion event, in physical
    /// pixels, for gesture deltas.
    last_cursor: Option<(f32, f32)>,
}

impl SurfaceWindow {
    /// Create a view window: a child of the main window when the platform
    /// allows one (D23's first surface kind), the detached top-level
    /// fallback otherwise.
    fn open(
        event_loop: &ActiveEventLoop,
        main: &Window,
        title: &str,
        kind: &str,
    ) -> Result<SurfaceWindow, EditorError> {
        // Step 1: prefer a child window, so the view is composited inside
        // the editor's content area. winit takes the parent as a raw
        // handle; without one there is no child to try. The end-to-end
        // suite forces this off (`PILL_EDITOR_DETACHED_SCENE=1`) to prove
        // the detached fallback path.
        let force_detached = std::env::var_os("PILL_EDITOR_DETACHED_SCENE").is_some();
        let child_attributes = (!force_detached).then(|| {
            main.window_handle().ok().map(|handle| {
                // SAFETY: the handle borrows the main window, which outlives
                // the view window: `EditorWindow` drops the surfaces before
                // the main window, and the renderer detaches before both.
                unsafe {
                    Window::default_attributes()
                        .with_title(title)
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
                        visible: true,
                        orbiting: false,
                        panning: false,
                        last_cursor: None,
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
        // every platform (the Dioxus editor's precedent).
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
            visible: true,
            orbiting: false,
            panning: false,
            last_cursor: None,
        })
    }

    /// Show or hide the window; returns whether this was a change.
    fn set_shown(&mut self, visible: bool) -> bool {
        if self.visible == visible {
            return false;
        }
        self.visible = visible;
        self.window.set_visible(visible);
        if !visible {
            self.orbiting = false;
            self.panning = false;
            self.last_cursor = None;
        }
        true
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

// =============================================================================
// EditorCamera
// =============================================================================

/// The editor's own camera for the Scene view, as plain data (D29).
///
/// Blender-like navigation around a pivot point: right-drag orbits, middle
/// drag pans, the wheel dollies, and W/S/A/D move the pivot forward or
/// sideways. The camera never enters the world - the scene surface is bound
/// to it as a [`CameraBinding::View`], which is what keeps project systems
/// from seeing or moving it.
struct EditorCamera {
    /// The point the camera looks at and orbits around.
    pivot: [f32; 3],
    /// Rotation around the world's Y axis, in radians.
    yaw: f32,
    /// Downward tilt, in radians; positive looks down at the pivot.
    pitch: f32,
    /// Distance from the pivot.
    distance: f32,
    /// A change since the binding was last published.
    dirty: bool,
}

impl Default for EditorCamera {
    fn default() -> Self {
        Self {
            pivot: [0.0, 0.0, 0.0],
            yaw: 0.0,
            pitch: 0.30,
            distance: 16.0,
            dirty: false,
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

    /// The camera's right direction, horizontal by construction.
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
        // `Ry(yaw) * Rx(-pitch)`: the yaw turns the world's Y axis, the
        // pitch tilts around the camera's own X axis.
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

    /// Take the dirty flag: whether anything changed since it was last
    /// taken.
    fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// Orbit around the pivot from a physical-pixel drag delta.
    fn orbit(&mut self, delta_x: f32, delta_y: f32) {
        self.yaw -= delta_x * 0.008;
        self.pitch = (self.pitch + delta_y * 0.008).clamp(-1.5, 1.5);
        self.changed("orbit");
    }

    /// Pan the pivot across the view plane from a drag delta; the drag
    /// moves the world with the cursor, so it scales with the distance.
    fn pan(&mut self, delta_x: f32, delta_y: f32) {
        let scale = self.distance * 0.0015;
        let right = self.right();
        let forward = self.forward();
        let up = [
            right[1] * forward[2] - right[2] * forward[1],
            right[2] * forward[0] - right[0] * forward[2],
            right[0] * forward[1] - right[1] * forward[0],
        ];
        for ((pivot, right_axis), up_axis) in self.pivot.iter_mut().zip(right).zip(up) {
            *pivot += (-right_axis * delta_x + up_axis * delta_y) * scale;
        }
        self.changed("pan");
        self.changed("pan");
    }

    /// Dolly the camera toward or away from the pivot; a wheel line is one
    /// unit, a pixel delta is fractional.
    fn dolly(&mut self, amount: f32) {
        self.distance = (self.distance * (1.0 - amount * 0.05)).clamp(0.5, 500.0);
        self.changed("dolly");
    }

    /// Move the pivot along the look direction; the step scales with the
    /// distance so moving feels the same at every zoom.
    fn fly(&mut self, direction: f32) {
        let step = (self.distance * 0.05).clamp(0.25, 5.0) * direction;
        let forward = self.forward();
        for (pivot, forward_axis) in self.pivot.iter_mut().zip(forward) {
            *pivot += forward_axis * step;
        }
        self.changed("fly");
    }

    /// Move the pivot sideways.
    fn strafe(&mut self, direction: f32) {
        let step = (self.distance * 0.05).clamp(0.25, 5.0) * direction;
        let right = self.right();
        for (pivot, right_axis) in self.pivot.iter_mut().zip(right) {
            *pivot += right_axis * step;
        }
        self.changed("strafe");
    }

    /// Mark a change and log it, so the end-to-end suite can prove the
    /// scene view moves the editor camera and nothing else.
    fn changed(&mut self, reason: &str) {
        self.dirty = true;
        info!(
            target: telemetry_target::ENGINE,
            reason,
            yaw = self.yaw,
            pitch = self.pitch,
            distance = self.distance,
            position = ?self.eye(),
            "[editor] editor camera moved"
        );
    }
}

/// Hamilton product of two `[x, y, z, w]` quaternions.
fn quaternion_product(left: [f32; 4], right: [f32; 4]) -> [f32; 4] {
    let [lx, ly, lz, lw] = left;
    let [rx, ry, rz, rw] = right;
    [
        lw * rx + lx * rw + ly * rz - lz * ry,
        lw * ry + ly * rw + lz * rx - lx * rz,
        lw * rz + lz * rw + lx * ry - ly * rx,
        lw * rw - lx * rx - ly * ry - lz * rz,
    ]
}

/// A cheap signature of the component registry's shape.
///
/// Both a removed type and a changed layout move it, which is exactly what
/// makes a history entry stale; rebuilding the layout list every frame would
/// cost more than the comparison.
fn component_registry_epoch(engine: &pill_engine_core::Engine) -> u64 {
    let world = engine.world();
    let registered = world.registered_components();
    let mut signature = registered.len() as u64;
    for (_, id) in &registered {
        let fields = world
            .component_field_layout(*id)
            .map(|layout| layout.len() as u64)
            .unwrap_or(0);
        signature = signature
            .wrapping_mul(31)
            .wrapping_add(fields.wrapping_add(7));
    }
    signature
}

/// The name after the last `::`, for logs and history labels.
fn short_component_name(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

/// Parse a scripted field value: `true`/`false`, a float (one with a `.`),
/// or a plain integer.
fn parse_field_value(token: &str) -> Result<pill_engine_core::FieldValue, String> {
    use pill_engine_core::FieldValue;
    match token {
        "true" => return Ok(FieldValue::Bool(true)),
        "false" => return Ok(FieldValue::Bool(false)),
        _ => {}
    }
    if token.contains('.') {
        if let Ok(value) = token.parse::<f32>() {
            return Ok(FieldValue::F32(value));
        }
    }
    if let Ok(value) = token.parse::<i32>() {
        return Ok(FieldValue::I32(value));
    }
    Err(format!("`{token}` is not a scriptable field value"))
}

/// Resolve a `NvG` display handle against the live entity table, so a script
/// can never address an entity the world does not hold.
fn resolve_entity(
    world: &pill_engine_core::World,
    display: &str,
) -> Option<pill_engine_core::Entity> {
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
fn resolve_component(world: &pill_engine_core::World, name: &str) -> Option<String> {
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
// Panels
// =============================================================================

/// The temporary Statistics panel: frame timing and a first interactive
/// row, proving the loop end to end until the shell module brings the real
/// panels.
fn statistics_panel(ui: &mut PanelBuilder<'_>, frames_per_second: f32, frame_count: u64) {
    ui.heading("Statistics");
    ui.separator();
    ui.label(&format!("{frames_per_second:.1} FPS"));
    ui.label(&format!("{frame_count} frames drawn"));
    ui.separator();
    ui.row(|ui| {
        ui.button("Reset");
        ui.button_enabled("Detach Scene", false);
    });
}
