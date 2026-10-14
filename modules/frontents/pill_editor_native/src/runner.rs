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
use pill_editor_api::{
    EditorNoticeEvent, EditorNotices, EditorOpenRequest, EditorOverlayLabel, EditorPopup,
    EditorProblem, EditorProblemLevel,
};
use pill_editor_api::{EditorRect, EditorShellFrame, EditorShellInput};
use pill_editor_core::editor_state::EditorCommand;
use pill_editor_core::gizmo::{self, GizmoMode, GizmoSettings, GizmoSpace, GizmoState};
use pill_editor_core::history::{CommandHistory, HistoryEntry};
use pill_engine_core::{ButtonState, InputEvent, KeyCode, Modifiers, MouseButton, ScrollDelta};
use pill_host::{HostConfig, HostNotice, RenderingHost};
use pill_input::winit_events::translate_window_event;
use pill_runtime::FrameDriver;
use pill_runtime::{
    CameraBinding, CameraComponent, PickHit, PickRequests, PickResults, SelectionOutlines,
    SurfaceId, SurfaceOptions, TransformComponent,
};
use pill_ui::panel::ClipboardRequest;
use pill_ui::{paint, run_frame};

// Current crate
use crate::platform;
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
use crate::overlay::{self, OverlaySurface};
use crate::popup::{PopupGpu, PopupHost};
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

/// How many toast events the shell's input may hold while nobody drains
/// them (the fallback UI, or a shell module that failed to load).
const TOAST_QUEUE_MAX: usize = 64;

/// How much of the camera distance the gizmo handles span, so they keep a
/// constant screen size (Stage 20).
const GIZMO_SCREEN_FRACTION: f32 = 0.18;

/// Where along a handle a scripted drag presses: inside the head, which the
/// pick buffer has a triangle for.
const GIZMO_PRESS_FRACTION: f32 = 0.92;

/// Frames between gizmo anchor heartbeats.
const GIZMO_HEARTBEAT_FRAMES: u64 = 210;

/// How far above its anchor the overlay's entity label is drawn, in logical
/// pixels (Stage 21).
const OVERLAY_LABEL_LIFT: f32 = 18.0;

/// Frames between overlay content heartbeats, for a suite that watches the
/// overlay path without a log line per frame (Stage 21).
const OVERLAY_HEARTBEAT_FRAMES: u64 = 120;

#[cfg(test)]
mod gizmo_tests {
    use super::*;

    /// A live entity handle: the commands the drag builds name one, and the
    /// world is the only place a handle comes from.
    fn entity() -> pill_engine_core::Entity {
        let mut world = pill_engine_core::World::new();
        world
            .create_entity()
            .build()
            .expect("an entity with no components")
    }

    /// The drag commands are the whole drop target of a gizmo move, so they
    /// are driven here the way a stub renderer's pick answer would drive
    /// them - no window, no GPU.
    #[test]
    fn a_translate_drag_writes_the_snapped_axis_value() {
        let original = TransformComponent {
            translation: [1.0, 2.0, 3.0],
            ..Default::default()
        };
        let mut settings = GizmoSettings::default();
        let (commands, value) = gizmo_drag_commands(
            GizmoMode::Translate,
            0,
            entity(),
            &original,
            1.5,
            &settings,
            2.0,
        );
        assert_eq!(commands.len(), 1);
        let EditorCommand::SetArrayElement {
            field,
            index,
            value: written,
            ..
        } = &commands[0]
        else {
            panic!("a translate drag writes one array element");
        };
        assert_eq!(field, "translation");
        assert_eq!(*index, 0);
        assert_eq!(value[0], 2.5);
        assert!(matches!(written, pill_engine_core::FieldValue::F32(v) if *v == 2.5));

        // Snapped, the same drag lands on the step.
        settings.snap = true;
        settings.translate_step = 0.5;
        let (_, value) = gizmo_drag_commands(
            GizmoMode::Translate,
            0,
            entity(),
            &original,
            1.7,
            &settings,
            2.0,
        );
        assert_eq!(value[0], 2.5, "1.0 + snap(1.7, 0.5) = 1.0 + 1.5");
    }

    /// A rotation drag writes all four quaternion elements, a scale drag its
    /// own single element, snapped to the step.
    #[test]
    fn rotation_and_scale_write_their_fields() {
        let original = TransformComponent::default();
        let mut settings = GizmoSettings::default();
        settings.snap = true;
        settings.rotate_step_degrees = 15.0;
        settings.scale_step = 0.5;
        let quarter = std::f32::consts::FRAC_PI_2;
        let (commands, value) = gizmo_drag_commands(
            GizmoMode::Rotate,
            1,
            entity(),
            &original,
            quarter,
            &settings,
            2.0,
        );
        assert_eq!(commands.len(), 4, "a rotation writes the whole quaternion");
        let expected = gizmo::axis_angle_quaternion(gizmo::AXES[1], std::f32::consts::FRAC_PI_2);
        for index in 0..4 {
            assert!(
                (value[index] - expected[index]).abs() < 1e-4,
                "element {index}: {} vs {}",
                value[index],
                expected[index]
            );
        }
        let scaled = TransformComponent {
            scale: [1.0, 1.0, 1.0],
            ..Default::default()
        };
        let (_, value) =
            gizmo_drag_commands(GizmoMode::Scale, 2, entity(), &scaled, 0.6, &settings, 2.0);
        assert_eq!(value[0], 1.5, "1.0 * (1 + 0.6/2) snapped to 0.5");
    }
}

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
pub fn run(log_buffer: pill_core::telemetry::capture::LogBuffer) -> Result<(), EditorError> {
    // Step 1: the event loop, polling rather than waiting so a frame is
    // always one redraw request away.
    let event_loop =
        EventLoop::new().map_err(|source| EditorError::EventLoopCreation { source })?;
    event_loop.set_control_flow(ControlFlow::Poll);
    // Step 2: run until the last window closes.
    let mut application = EditorApplication {
        log_buffer: Some(log_buffer),
        ..EditorApplication::default()
    };
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
    /// The capture ring the console panel reads; installed with the
    /// telemetry stack before the loop starts.
    log_buffer: Option<pill_core::telemetry::capture::LogBuffer>,
    /// A failure recorded outside the event callbacks' return path;
    /// surfaced by [`run`] after the loop exits.
    failure: Option<EditorError>,
}

impl ApplicationHandler for EditorApplication {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.editor.is_some() {
            return;
        }
        match EditorWindow::open(event_loop, self.log_buffer.clone()) {
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
        // The popup windows (Stage 21, D23) are their own windows: a close
        // request, a lost focus or Escape dismisses one, and a click inside
        // one is a pointer for the menu drawn in it.
        if editor.is_popup_window(window_id) {
            editor.popup_window_event(window_id, &event);
            return;
        }
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
                // The popups are owned windows and would die with it anyway;
                // closing them first keeps the shutdown ordered.
                editor.popups.close_all();
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                editor.resize(size.width, size.height);
            }
            // The OS drag-and-drop events (Stage 18): hover state feeds the
            // shell's drop preview; a dropped file becomes an import.
            WindowEvent::HoveredFile(path) => {
                editor.hover_file(path);
            }
            WindowEvent::HoveredFileCancelled => {
                editor.clear_hovered_files();
            }
            WindowEvent::DroppedFile(path) => {
                editor.drop_file(path);
            }
            // A press that began on an asset row and then left the window is
            // a drag out (Stage 18): OLE takes over and the file travels as
            // `CF_HDROP`. While the button is down Windows keeps delivering
            // mouse moves to the window that captured it, and a `CursorLeft`
            // never arrives, so the position itself is the signal; the row's
            // held state reached the executable as the last frame's output.
            WindowEvent::CursorMoved { position, .. } => {
                editor.drag_out_if_past_the_edge(&position);
                editor.push_ui_input(&event);
            }
            WindowEvent::CursorLeft { .. } => {
                editor.drag_out_if_past_the_edge(&winit::dpi::PhysicalPosition::new(-1.0, -1.0));
                editor.push_ui_input(&event);
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

    /// The frame boundary between event batches: where the popups the
    /// shell's last frame asked for become real windows (Stage 21, D23).
    ///
    /// Creating a window re-enters winit's dispatch, so it never happens
    /// inside an event callback; the shell's request is applied here, one
    /// step ahead of the frame that draws it.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        editor.sync_popups(event_loop);
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
    /// The instance and adapter the device came from: a popup window's
    /// surface is created and checked against them (Stage 21, D23).
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
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
    /// A module panel the scripted suite asked to focus or open, by id;
    /// delivered to the shell with the next frame's input.
    requested_panel: Option<String>,
    /// A widget the scripted suite asked to activate, by name; delivered to
    /// the shell with the next frame's input.
    requested_widget: Option<String>,
    /// Lines to wheel-scroll at the last activated widget; delivered to the
    /// shell with the next frame's input.
    requested_wheel: Option<f32>,
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
    /// The asset browser's rows, refreshed periodically (the walk touches
    /// the filesystem).
    asset_entries: Vec<pill_editor_core::editor_state::AssetListing>,
    /// The entity tier's change tracking: the entity count it was built
    /// for, the frames since, and whether an applied edit made it stale.
    /// Its per-entity strings make a per-frame rebuild too expensive on a
    /// big world, so the tier follows these signals instead.
    entity_tier_count: usize,
    entity_tier_age: u32,
    entity_tier_dirty: bool,
    /// Signature of the component registry at the last check; a reload that
    /// changes it invalidates stale history entries.
    registry_epoch: u64,
    /// Frame pacing for the displayed FPS.
    last_frame: Instant,
    frames_per_second: f32,
    frame_count: u64,
    /// The clipboard read counter, so the shell matches its pending paste to
    /// the answer it asked for (Stage 18).
    clipboard_seq: u64,
    /// Files the OS drag is hovering over the window (Stage 18).
    hovered_files: Vec<String>,
    /// Files the OS dropped, drained into the shell's input next frame.
    dropped_files: Vec<String>,
    /// The project's asset root, the import's destination.
    asset_root: Option<String>,
    /// An asset row held down in the shell's last frame, by its source path
    /// relative to the asset root (Stage 18); a drag that leaves the window
    /// hands this file to the OS.
    drag_candidate: Option<String>,
    /// A drag waiting for the frame boundary. OLE's modal loop must not run
    /// inside winit's event dispatch: a drop back on the editor's own window
    /// re-enters that dispatch mid-callback and the loop stalls afterwards.
    /// The scripted drag-out verb runs at the frame boundary and never
    /// stalls, so the real gesture goes through the same door.
    drag_request: Option<String>,
    /// Whether the left mouse button is down as the real window events
    /// reported it; a scripted press never sets it, so a scripted click
    /// cannot start a drag out.
    mouse_left_held: bool,
    /// The Assets panel's rectangle, from the shell's last frame; a
    /// scripted drop aims its pointer at it.
    last_assets_rect: Option<EditorRect>,
    /// The problems and toast events the editor shows (Stage 19), folded
    /// from the host's notices; the shell's input carries what it draws
    /// from and drains.
    notices: EditorNotices,
    /// The pick this executable is waiting for (Stage 20), by its request
    /// id; answers land frames later and match by id.
    pending_pick: Option<u64>,
    /// Where the left mouse button went down on the scene window, in
    /// physical pixels, so its release can ask what is under it. A real
    /// click only; the scripted channel requests its pick directly.
    scene_press: Option<(f32, f32)>,
    /// The entity the outlines currently name, so the world resource is
    /// rewritten only on a change.
    outlined_entity: Option<pill_engine_core::Entity>,
    /// The gizmo drag in flight, if any (Stage 20).
    gizmo_drag: Option<GizmoDrag>,
    /// The project this editor opened, by directory name: the key the gizmo
    /// preferences are remembered under.
    gizmo_project: Option<String>,
    /// The gizmo settings last written to the preferences file, so the write
    /// happens on a change instead of every frame.
    gizmo_persisted: Option<GizmoSettings>,
    /// The gizmo header (mode, space, anchor) last logged, so it logs on a
    /// change rather than every frame.
    gizmo_logged: Option<(GizmoMode, GizmoSpace, i32, i32)>,
    /// A scripted gizmo drag waiting to start (test channel, Stage 20): the
    /// axis to grab and the pixel delta to drag by.
    synthetic_gizmo_drag: Option<(usize, f32, f32)>,
    /// The pixel delta of the scripted drag whose pick is in flight; `None`
    /// for a press the real mouse issued.
    synthetic_gizmo_pending: Option<(f32, f32)>,
    /// The scripted press's pixel neighbourhood: the request ids still in
    /// flight, so the first pixel that names a handle starts the drag and a
    /// full miss refuses it.
    synthetic_press_ids: Vec<u64>,
    /// Counter naming each drag's coalesce key, so one drag is one undo step
    /// and the next drag is a new one.
    gizmo_drag_seq: u64,
    /// The composition overlay over the window (Stage 21, D24), `None` when
    /// it could not be created: the shell's overlay list then draws through
    /// the renderer's UI pass into the scene instead.
    overlay: Option<OverlaySurface>,
    /// Whether the overlay was refused on purpose (`PILL_EDITOR_NO_OVERLAY`),
    /// so the fallback path stays exercised.
    overlay_forced_failure: bool,
    /// The overlay list the shell built last frame, waiting for its draw:
    /// the composition overlay takes it at the end of the frame, and without
    /// one the world's UI context takes it at the start of the next.
    pending_overlay: Option<DrawList>,
    /// The overlay's opaque rectangles, from the shell's last frame: input
    /// over one of them belongs to the UI, not to the scene beneath it.
    overlay_opaque: Vec<EditorRect>,
    /// The gizmo labels the overlay shows this frame, published by the
    /// executable because only it knows where the scene projects.
    overlay_labels: Vec<EditorOverlayLabel>,
    /// The shell's open menus, combos and context menus as platform popups
    /// (Stage 21, D23).
    popups: PopupHost,
    /// The popups the shell asked for in its last frame, awaiting the frame
    /// boundary: `Some` until `sync_popups` has applied them, `None` after.
    /// An empty list and `None` mean different things - the list closes the
    /// open popups, `None` leaves them alone - so a boundary pass over a
    /// frame whose requests are already applied never tears them down.
    requested_popups: Option<Vec<EditorPopup>>,
    /// The popups the platform closed since the shell's last frame (an
    /// outside click, a lost focus, Escape), by id.
    popup_closed: Vec<String>,
    /// The overlay list's vertex count at the last heartbeat log, so the
    /// overlay's path and content log on a change rather than every frame.
    overlay_vertices_logged: usize,
}

impl EditorWindow {
    /// Create the window and its GPU state, and log the startup line the
    /// harness waits for.
    fn open(
        event_loop: &ActiveEventLoop,
        log_buffer: Option<pill_core::telemetry::capture::LogBuffer>,
    ) -> Result<EditorWindow, EditorError> {
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
        // The editor's device draws the composition overlay, whose surface
        // wgpu implements on DX12 alone (Stage 21, D24): Windows defaults to
        // DX12 so the overlay exists without an override, WGPU_BACKEND still
        // wins, and a backend set without DX12 refuses the overlay into the
        // scene-pass fallback. Elsewhere the primary backends stay - never a
        // hidden GL helper window that outlives the code that created it
        // (Stage 31 gives the Linux overlay its own surfaces, not this one).
        let default_backends = if cfg!(windows) {
            wgpu::Backends::DX12
        } else {
            wgpu::Backends::PRIMARY
        };
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::from_env().unwrap_or(default_backends),
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
        // Step 4b: the composition overlay over the window (Stage 21, D24).
        // Its list floats above the scene surfaces with per-pixel alpha;
        // when the platform refuses it, or the scripted suite forces that
        // refusal, the same list draws through the renderer's UI pass into
        // the scene instead.
        let overlay_forced_failure = std::env::var_os("PILL_EDITOR_NO_OVERLAY").is_some();
        let overlay = if overlay_forced_failure {
            warn!(
                target: telemetry_target::ENGINE,
                "[editor] overlay disabled by PILL_EDITOR_NO_OVERLAY; the UI falls back to the scene pass"
            );
            None
        } else {
            match OverlaySurface::new(
                &instance,
                &adapter,
                &device,
                Arc::clone(&window),
                format,
                (width, height),
                window.scale_factor() as f32,
            ) {
                Ok(surface) => {
                    info!(
                        target: telemetry_target::ENGINE,
                        width, height, "[editor] overlay created"
                    );
                    Some(surface)
                }
                Err(error) => {
                    warn!(
                        target: telemetry_target::ENGINE,
                        error = %error,
                        "[editor] overlay refused; the UI falls back to the scene pass"
                    );
                    None
                }
            }
        };
        // The monitor's work area, in physical pixels: what the popup
        // placement flips and slides inside (Stage 21). Logged once because
        // the platform is the only thing that knows it, and the D26 suite
        // places a popup at an edge to observe the flip.
        if let Some(work) = overlay::work_area(&window) {
            info!(
                target: telemetry_target::ENGINE,
                x = work.x,
                y = work.y,
                width = work.width,
                height = work.height,
                scale = window.scale_factor(),
                "[editor] monitor work area"
            );
        }
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
        // The import destination for dropped files: resolved once, the
        // project does not move mid-run.
        let asset_root = host
            .host()
            .asset_directory()
            .map(|path| path.to_string_lossy().into_owned());
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
            Some(project) => pill_editor_core::commands::read_gizmo_settings(
                &pill_editor_core::layout::layout_directory(),
                project,
            ),
            None => GizmoSettings::default(),
        };
        host.host_mut()
            .engine_mut()
            .world_mut()
            .insert_resource(GizmoState {
                settings: gizmo_settings,
            });
        // The console's capture ring, for the shell's Console panel (Stage
        // 17); present even when telemetry setup failed, so the panel
        // always has a reader.
        if let Some(log_buffer) = log_buffer {
            host.host_mut()
                .engine_mut()
                .world_mut()
                .insert_resource(pill_editor_api::EditorLogs(log_buffer));
        }
        // The clipboard answer resource (Stage 18): the shell's system
        // takes it as a parameter from the first frame, so it must exist
        // before the shell runs even though the first read lands later.
        host.host_mut()
            .engine_mut()
            .world_mut()
            .insert_resource(pill_editor_api::EditorClipboardAnswer::default());
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
        // The scripted suite's panel fixtures: a panel that panics, so the
        // shell's containment can be exercised end to end (Stage 16).
        // Registered only while the scripted channel is active; its name
        // avoids the word the native suite's no-crash assertion scans for.
        if std::env::var_os("PILL_EDITOR_TEST_COMMANDS").is_some() {
            pill_editor_api::panels::register_editor_panel(
                host.host_mut().engine_mut(),
                "editor_test",
                "containment_test",
                "Containment Test",
                containment_test_panel,
            );
        }
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
            instance,
            adapter,
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
            requested_panel: None,
            requested_widget: None,
            requested_wheel: None,
            requested_commands: Vec::new(),
            test_channel: TestChannel::from_environment(),
            test_started: Some(Instant::now()),
            test_seconds: test_channel::seconds_from_environment(),
            quit_requested: false,
            asset_entries: Vec::new(),
            entity_tier_count: usize::MAX,
            entity_tier_age: 0,
            entity_tier_dirty: false,
            registry_epoch: 0,
            last_frame: Instant::now(),
            frames_per_second: 0.0,
            frame_count: 0,
            clipboard_seq: 0,
            hovered_files: Vec::new(),
            dropped_files: Vec::new(),
            asset_root,
            last_assets_rect: None,
            drag_candidate: None,
            drag_request: None,
            mouse_left_held: false,
            notices: EditorNotices::default(),
            pending_pick: None,
            scene_press: None,
            outlined_entity: None,
            gizmo_drag: None,
            gizmo_project,
            gizmo_persisted: Some(gizmo_settings),
            gizmo_logged: None,
            synthetic_gizmo_drag: None,
            synthetic_gizmo_pending: None,
            synthetic_press_ids: Vec::new(),
            gizmo_drag_seq: 0,
            overlay,
            overlay_forced_failure,
            pending_overlay: None,
            overlay_opaque: Vec::new(),
            overlay_labels: Vec::new(),
            popups: PopupHost::new(),
            requested_popups: None,
            popup_closed: Vec::new(),
            overlay_vertices_logged: usize::MAX,
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
        // The overlay's visual covers the client area (D24): its swapchain
        // follows the window in physical pixels.
        if let Some(overlay) = self.overlay.as_mut() {
            overlay.resize(&self.device, (width, height));
        }
        // A resize redraws immediately; the scene surfaces follow through
        // the host's own resize path.
        self.window.request_redraw();
    }

    /// Queue one `winit` event's engine form into the UI input, in logical
    /// pixels.
    fn push_ui_input(&mut self, event: &WindowEvent) {
        // The real button state, for the drag-out trigger: `CursorLeft`
        // carries no button information of its own.
        if let WindowEvent::MouseInput { state, button, .. } = event {
            if *button == winit::event::MouseButton::Left {
                self.mouse_left_held = *state == winit::event::ElementState::Pressed;
            }
        }
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
        self.state.end_frame();
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

    /// Whether `id` is one of the shell's popup windows (Stage 21, D23).
    fn is_popup_window(&self, id: WindowId) -> bool {
        self.popups.is_popup(id)
    }

    /// Handle one event of one popup window: dismissal, and the pointers the
    /// shell's menus read.
    fn popup_window_event(&mut self, id: WindowId, event: &WindowEvent) {
        // A popup is a window of its own, so its keyboard input never
        // reaches the main window's stream: translate it here, into the
        // same queue the shell's text fields read from, and let the popup
        // host decide what dismisses it.
        if matches!(
            event,
            WindowEvent::KeyboardInput { .. } | WindowEvent::Ime(_)
        ) {
            self.pending_ui_events.extend(translate_window_event(event));
        }
        if !self.popups.window_event(id, event) {
            return;
        }
        // Any popup input means the popups are on screen; the next frame
        // draws them and the shell's menu state follows.
        self.window.request_redraw();
    }

    /// Turn the shell's popup requests into real windows, at the frame
    /// boundary (Stage 21, D23): opening, moving and closing them, and
    /// recording the ids the platform closed on its own.
    fn sync_popups(&mut self, event_loop: &ActiveEventLoop) {
        // The boundary runs once per event batch, several times a frame; a
        // request list is applied exactly once, so the passes after it see
        // `None` and leave the windows alone.
        let Some(requested) = self.requested_popups.take() else {
            return;
        };
        let gpu = PopupGpu {
            instance: &self.instance,
            adapter: &self.adapter,
            device: &self.device,
            format: self.configuration.format,
        };
        let closed = self.popups.sync(event_loop, &gpu, &self.window, &requested);
        // A popup the platform closed is dropped by `sync`; its id tells the
        // shell to clear the state behind it next frame.
        self.popup_closed.extend(closed);
        if !self.popups.is_empty() {
            self.window.request_redraw();
        }
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
                    TestCommand::RightClick { x, y } => self.inject_right_click(x, y),
                    TestCommand::PopupPointer { id, x, y, released } => {
                        let position = Vector2f::new(x, y);
                        if self.popups.push_pointer(&id, position, released) {
                            info!(
                                target: telemetry_target::ENGINE,
                                id = %id,
                                x,
                                y,
                                released,
                                "[editor] test popup pointer"
                            );
                        } else {
                            warn!(
                                target: telemetry_target::ENGINE,
                                id = %id,
                                "[editor] test popup pointer: no such popup"
                            );
                        }
                    }
                    TestCommand::PopupDismiss { id } => {
                        if self.popups.dismiss(&id) {
                            info!(
                                target: telemetry_target::ENGINE,
                                id = %id,
                                "[editor] test popup dismiss"
                            );
                        } else {
                            warn!(
                                target: telemetry_target::ENGINE,
                                id = %id,
                                "[editor] test popup dismiss: no such popup"
                            );
                        }
                    }
                    TestCommand::WindowMove { x, y } => {
                        info!(
                            target: telemetry_target::ENGINE,
                            x,
                            y,
                            "[editor] test window move"
                        );
                        self.window
                            .set_outer_position(winit::dpi::PhysicalPosition::new(x, y));
                    }
                    TestCommand::Key { key, target } => self.inject_key(&key, target.as_deref()),
                    TestCommand::Tab { title } => {
                        info!(
                            target: telemetry_target::ENGINE,
                            title = %title,
                            "[editor] test channel: tab request"
                        );
                        self.requested_tab = Some(title);
                    }
                    TestCommand::Panel { id } => {
                        info!(
                            target: telemetry_target::ENGINE,
                            id = %id,
                            "[editor] test channel: panel request"
                        );
                        self.requested_panel = Some(id);
                    }
                    TestCommand::Activate { name } => {
                        info!(
                            target: telemetry_target::ENGINE,
                            name = %name,
                            "[editor] test channel: activate request"
                        );
                        self.requested_widget = Some(name);
                    }
                    TestCommand::Select { entity } => self.select_entity_display(&entity),
                    TestCommand::Wheel { lines } => {
                        info!(
                            target: telemetry_target::ENGINE,
                            lines,
                            "[editor] test channel: wheel request"
                        );
                        self.requested_wheel = Some(lines);
                    }
                    TestCommand::Type { text } => {
                        info!(
                            target: telemetry_target::ENGINE,
                            text = %text,
                            "[editor] test channel: type request"
                        );
                        self.pending_ui_events.push(InputEvent::TextInput { text });
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
                    TestCommand::HoverFile { path } => {
                        info!(
                            target: telemetry_target::ENGINE,
                            path = %path,
                            "[editor] test channel: hover-file request"
                        );
                        self.aim_at_assets();
                        self.hover_file(std::path::PathBuf::from(path));
                    }
                    TestCommand::DropFile { path } => {
                        info!(
                            target: telemetry_target::ENGINE,
                            path = %path,
                            "[editor] test channel: drop-file request"
                        );
                        self.aim_at_assets();
                        self.drop_file(std::path::PathBuf::from(path));
                    }
                    TestCommand::DragOut { path } => {
                        info!(
                            target: telemetry_target::ENGINE,
                            path = %path,
                            "[editor] test channel: drag-out request"
                        );
                        self.start_drag_out(&path);
                    }
                    TestCommand::GizmoDrag { axis, dx, dy } => {
                        info!(
                            target: telemetry_target::ENGINE,
                            axis,
                            dx,
                            dy,
                            "[editor] test channel: gizmo drag request"
                        );
                        self.synthetic_gizmo_drag = Some((axis, dx, dy));
                    }
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
            test_channel::EditCommand::System { index, enabled } => {
                label = format!("system {index} {}", if *enabled { "on" } else { "off" });
                EditorCommand::SetSystemEnabled {
                    index: *index,
                    enabled: *enabled,
                }
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
                let resolved =
                    resolve_entity(world, &entity).zip(resolve_component(world, &component));
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

    /// The scripted selection: the same shared resource a Hierarchy click
    /// sets, resolved against the live entity table.
    fn select_entity_display(&mut self, handle: &str) {
        let engine = self.host.host_mut().engine_mut();
        let world = engine.world_mut();
        let Some(entity) = resolve_entity(world, handle) else {
            warn!(
                target: telemetry_target::ENGINE,
                handle = %handle,
                "[editor] test channel: selection target not resolvable"
            );
            return;
        };
        if let Some(selection) =
            world.get_resource_mut::<pill_editor_core::editor_state::EditorSelection>()
        {
            selection.entity = Some(entity);
        }
        info!(
            target: telemetry_target::ENGINE,
            entity = %entity,
            "[editor] selection changed"
        );
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
        // The overlay's opaque regions are the UI's even where they sit over
        // the scene view (Stage 21, D24): a click on the FPS badge reaches
        // `pill_ui`, one beside it reaches the scene.
        let overlay_target = self.overlay_opaque.iter().any(|rect| {
            x >= rect.x && x <= rect.x + rect.width && y >= rect.y && y <= rect.y + rect.height
        });
        let game_target = !overlay_target && in_rect(self.last_game_rect);
        let scene_target = !overlay_target && !game_target && in_rect(self.last_scene_rect);
        // A click on the editor's own window is a click outside every popup,
        // so the platform takes their focus away and closes them; the shell
        // hears about it through the ids the next sync reports.
        self.popups.dismiss_all();
        info!(
            target: telemetry_target::ENGINE,
            x,
            y,
            target = if overlay_target {
                "ui"
            } else if game_target {
                "game"
            } else if scene_target {
                "scene"
            } else {
                "ui"
            },
            overlay = overlay_target,
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
            // The scene view's click asks the renderer what is under the
            // pixel (Stage 20); the answer selects, frames later.
            info!(
                target: telemetry_target::ENGINE,
                x,
                y,
                "[editor] scene view click"
            );
            let scale_factor = self.window.scale_factor() as f32;
            if let Some(rect) = self.last_scene_rect {
                let physical_x = ((x - rect.x) * scale_factor).max(0.0).round() as u32;
                let physical_y = ((y - rect.y) * scale_factor).max(0.0).round() as u32;
                self.request_scene_pick(physical_x, physical_y);
            }
        } else {
            for event in presses(Vector2f::new(x, y)) {
                self.state.input.apply(&event);
                self.pending_ui_events.push(event);
            }
        }
    }

    /// A scripted right-click: press and release the right button over the
    /// client area, which is how the shell is asked for a context menu
    /// (Stage 21). The view surfaces are windows of their own, so the event
    /// goes to the UI's stream, exactly as a real right click here would.
    fn inject_right_click(&mut self, x: f32, y: f32) {
        info!(
            target: telemetry_target::ENGINE,
            x,
            y,
            "[editor] test right click"
        );
        let position = Vector2f::new(x, y);
        let events = [
            InputEvent::CursorMoved { position },
            InputEvent::MouseButton {
                button: MouseButton::Right,
                state: ButtonState::Pressed,
            },
            InputEvent::MouseButton {
                button: MouseButton::Right,
                state: ButtonState::Released,
            },
        ];
        for event in events {
            self.state.input.apply(&event);
            self.pending_ui_events.push(event);
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
        if let WindowEvent::Resized(size) = event {
            // The game surface is the renderer's primary one; its window
            // resize must reach the swapchain, like any frontend forwards
            // its window's (Stage 20 found the gap through scene picking).
            self.game.placed = None;
            self.host.resize(size.width.max(1), size.height.max(1));
        }
        for event in translate_window_event(event) {
            self.host.push_input(event);
        }
    }

    /// Route one scene-window event into the editor camera (D29): the scene
    /// view never forwards input to the game. Orbit on right drag, pan on
    /// middle drag, dolly on the wheel, and WASD moves the focus point.
    fn scene_view_event(&mut self, event: &WindowEvent) {
        if let WindowEvent::Resized(size) = event {
            // The scene surface reconfigures with its window: the render
            // target is what pick pixels are counted in, and a stale target
            // sends every pick to the wrong texel (Stage 20).
            self.scene.placed = None;
            if self.scene_attached {
                self.host
                    .resize_surface(SCENE_SURFACE, size.width.max(1), size.height.max(1));
            }
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
                // A gizmo drag follows the cursor (Stage 20); the move is a
                // re-read of the ray, not an accumulation of deltas.
                if self.gizmo_drag.is_some() {
                    self.update_gizmo_drag(position.x as f32, position.y as f32);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => match button {
                winit::event::MouseButton::Right => {
                    self.scene.orbiting = *state == winit::event::ElementState::Pressed;
                }
                winit::event::MouseButton::Middle => {
                    self.scene.panning = *state == winit::event::ElementState::Pressed;
                }
                // A left click asks what is under the pixel (Stage 20). The
                // release does the asking, so a press that turned into a
                // scene drag is not read as a click; the position is the
                // scene window's own client area, which is what a pick
                // wants.
                winit::event::MouseButton::Left => match *state {
                    winit::event::ElementState::Pressed => {
                        self.scene_press = self.scene.last_cursor;
                    }
                    winit::event::ElementState::Released => {
                        // A release while a gizmo drag runs ends the drag; the
                        // press that started it already asked its pick.
                        if self.gizmo_drag.is_some() {
                            self.end_gizmo_drag();
                        } else if let Some((x, y)) = self.scene_press.take() {
                            self.request_scene_pick(x as u32, y as u32);
                        }
                    }
                },
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

    /// A file the OS drag brought over the window (Stage 18); recorded until
    /// the drag ends or the file drops.
    fn hover_file(&mut self, path: std::path::PathBuf) {
        let file = path.to_string_lossy().into_owned();
        info!(
            target: telemetry_target::ENGINE,
            path = file.as_str(),
            "[editor] file hovered"
        );
        self.hovered_files.push(file);
    }

    /// The drag left the window without dropping.
    fn clear_hovered_files(&mut self) {
        if !self.hovered_files.is_empty() {
            self.hovered_files.clear();
            info!(
                target: telemetry_target::ENGINE,
                "[editor] file hover cleared"
            );
        }
    }

    /// A file was dropped on the window (Stage 18); the shell decides the
    /// target and queues the import.
    fn drop_file(&mut self, path: std::path::PathBuf) {
        let file = path.to_string_lossy().into_owned();
        info!(
            target: telemetry_target::ENGINE,
            path = file.as_str(),
            "[editor] file dropped"
        );
        self.hovered_files.clear();
        self.dropped_files.push(file);
    }

    /// Turn the host's queued notices into the shell's notice resource
    /// (Stage 19).
    ///
    /// Every notice becomes one toast event; a failed reload or patch also
    /// becomes Problems rows, parsed from the captured build text where the
    /// notice carries one. A module's rows are replaced by its next
    /// successful reload - that is what clears the panel once the error is
    /// fixed - and by nothing else, so a module that keeps failing keeps its
    /// list.
    fn publish_host_notices(&mut self) {
        let notices = pill_host::take_notices();
        if notices.is_empty() {
            return;
        }
        for notice in notices {
            let (level, text) = notice_toast(&notice);
            info!(
                target: telemetry_target::ENGINE,
                level = notice_level_name(level),
                text = text.as_str(),
                "[editor] toast"
            );
            let seq = self.notices.next_event_seq;
            self.notices.next_event_seq = seq + 1;
            self.notices
                .events
                .push(EditorNoticeEvent { seq, level, text });
            match &notice {
                HostNotice::ModuleFailed { module, output } => {
                    self.notices
                        .problems
                        .retain(|problem| problem.module != *module);
                    let rows = problems_from_build_output(module, output);
                    let count = rows.len();
                    // One line per parsed problem (bounded), so the log tells
                    // the suite - and anyone debugging - what the panel holds
                    // and where each row would jump to. The field is `text`,
                    // not `message`: tracing reserves `message` for the event
                    // line itself.
                    for row in rows.iter().take(10) {
                        info!(
                            target: telemetry_target::ENGINE,
                            module = module.as_str(),
                            level = notice_level_name(row.level),
                            file = row.file.as_deref().unwrap_or(""),
                            line = row.line.unwrap_or(0),
                            text = row.message.as_str(),
                            "[editor] problem"
                        );
                    }
                    self.notices.problems.extend(rows);
                    info!(
                        target: telemetry_target::ENGINE,
                        module = module.as_str(),
                        count,
                        "[editor] problems updated"
                    );
                }
                HostNotice::Reloaded { module, .. } => {
                    let before = self.notices.problems.len();
                    self.notices
                        .problems
                        .retain(|problem| problem.module != *module);
                    if self.notices.problems.len() != before {
                        info!(
                            target: telemetry_target::ENGINE,
                            module = module.as_str(),
                            count = 0,
                            "[editor] problems updated"
                        );
                    }
                }
                HostNotice::PatchFailed { function, detail } => {
                    self.notices
                        .problems
                        .retain(|problem| problem.module != *function);
                    self.notices.problems.push(EditorProblem {
                        module: function.clone(),
                        level: EditorProblemLevel::Error,
                        message: detail.clone(),
                        file: None,
                        line: None,
                        column: None,
                    });
                    info!(
                        target: telemetry_target::ENGINE,
                        module = function.as_str(),
                        count = 1,
                        "[editor] problems updated"
                    );
                }
                HostNotice::RolledBack {
                    function,
                    to_generation,
                    automatic,
                } => {
                    // A rollback is worth a Problems row too (Stage 19): the
                    // running implementation no longer matches the sources
                    // until the next good edit.
                    self.notices
                        .problems
                        .retain(|problem| problem.module != *function);
                    let message = if *automatic {
                        format!("rolled back to generation {to_generation} after repeated panics")
                    } else {
                        format!("rolled back to generation {to_generation}")
                    };
                    self.notices.problems.push(EditorProblem {
                        module: function.clone(),
                        level: EditorProblemLevel::Warning,
                        message,
                        file: None,
                        line: None,
                        column: None,
                    });
                    info!(
                        target: telemetry_target::ENGINE,
                        module = function.as_str(),
                        count = 1,
                        "[editor] problems updated"
                    );
                }
                HostNotice::PatchApplied { .. } => {}
            }
            self.notices.revision = self.notices.revision.wrapping_add(1);
        }
    }

    /// Ask the renderer what is under one physical pixel of the scene
    /// surface (Stage 20).
    ///
    /// The answer never stalls a frame: the request rides the world
    /// resource into the next render, and [`Self::drain_scene_picks`] applies
    /// whatever comes back, matched by request id.
    fn request_scene_pick(&mut self, physical_x: u32, physical_y: u32) {
        self.pending_pick = self.queue_scene_pick(physical_x, physical_y);
    }

    /// Queue one scene pick and return its id, leaving the answer to
    /// [`Self::drain_scene_picks`].
    fn queue_scene_pick(&mut self, physical_x: u32, physical_y: u32) -> Option<u64> {
        let world = self.host.host_mut().engine_mut().world_mut();
        if world.get_resource::<PickRequests>().is_none() {
            world.insert_resource(PickRequests::default());
        }
        let requests = world.get_resource_mut::<PickRequests>()?;
        let id = requests.request(SCENE_SURFACE, physical_x, physical_y);
        info!(
            target: telemetry_target::ENGINE,
            x = physical_x,
            y = physical_y,
            id,
            "[editor] pick requested"
        );
        Some(id)
    }

    /// Publish the selection as the renderer's outline set (Stage 20).
    ///
    /// One resource write per change: the world's [`SelectionOutlines`] is
    /// what the renderer reads to outline, and the executable owns it
    /// because it owns the world's frame order - the selection changes in
    /// many places (Hierarchy, Inspector, scene picks) and the outline
    /// belongs to all of them.
    fn publish_selection_outline(&mut self) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let selected = world
            .get_resource::<pill_editor_core::editor_state::EditorSelection>()
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

    /// Apply the answers to scene picks that arrived since the last frame
    /// (Stage 20): an entity selects everywhere, empty space clears, and a
    /// gizmo handle is recorded for the drag whose press asked for it.
    fn drain_scene_picks(&mut self) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let results = match world.get_resource_mut::<PickResults>() {
            Some(results) => results.take(),
            None => return,
        };
        // A handle press starts a drag through methods that borrow all of
        // `self`, while the world borrow below is still alive; the press is
        // collected here and acted on once that borrow ends. The recorded
        // press: the handle id, the press position, and the scripted delta
        // when the press was synthetic.
        type HandlePress = (u32, f32, f32, Option<(f32, f32)>);
        let mut handle_press: Option<HandlePress> = None;
        for result in results {
            // A scripted press answers on any pixel of its neighbourhood:
            // the first that names a handle starts the drag, and only a full
            // miss refuses it. An entity under one of those pixels is not a
            // selection - the press was aimed at a handle.
            if let Some(index) = self
                .synthetic_press_ids
                .iter()
                .position(|id| *id == result.request.id)
            {
                self.synthetic_press_ids.remove(index);
                match result.hit {
                    PickHit::Handle(handle) => {
                        if let Some(delta) = self.synthetic_gizmo_pending.take() {
                            info!(
                                target: telemetry_target::ENGINE,
                                handle,
                                "[editor] scene pick handle"
                            );
                            handle_press = Some((
                                handle,
                                result.request.x as f32,
                                result.request.y as f32,
                                Some(delta),
                            ));
                        }
                    }
                    _ => {
                        if self.synthetic_press_ids.is_empty()
                            && self.synthetic_gizmo_pending.is_some()
                        {
                            self.synthetic_gizmo_pending = None;
                            warn!(
                                target: telemetry_target::ENGINE,
                                "[editor] gizmo drag refused: the press point hit no handle"
                            );
                        }
                    }
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
                    let changed = match world
                        .get_resource_mut::<pill_editor_core::editor_state::EditorSelection>()
                    {
                        Some(selection) => {
                            let changed = selection.entity != Some(entity);
                            selection.entity = Some(entity);
                            changed
                        }
                        None => false,
                    };
                    // The same token the shell logs when a panel selects, so
                    // a suite reads one shape for "the selection moved".
                    if changed {
                        info!(
                            target: telemetry_target::ENGINE,
                            entity = %entity,
                            "[editor] selection changed"
                        );
                    }
                }
                PickHit::Nothing => {
                    info!(
                        target: telemetry_target::ENGINE,
                        "[editor] scene pick nothing"
                    );
                    if let Some(selection) =
                        world.get_resource_mut::<pill_editor_core::editor_state::EditorSelection>()
                    {
                        selection.entity = None;
                    }
                }
                PickHit::Handle(handle) => {
                    info!(
                        target: telemetry_target::ENGINE,
                        handle,
                        "[editor] scene pick handle"
                    );
                    let scripted = self.synthetic_gizmo_pending.take();
                    handle_press = Some((
                        handle,
                        result.request.x as f32,
                        result.request.y as f32,
                        scripted,
                    ));
                }
            }
        }
        if let Some((handle, press_x, press_y, scripted)) = handle_press {
            self.begin_gizmo_drag(handle, press_x, press_y);
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

    /// Draw the gizmo handles for the selection (Stage 20).
    ///
    /// The contract's own primitives, handle ids set: the same frame the
    /// renderer draws is the frame it picks, so a handle a drag grabs is
    /// always the handle that was on screen. Handles keep a constant screen
    /// size (their world length follows the camera distance), and the frame
    /// they are drawn into is one frame's worth - nothing to clear here.
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
                .get_resource::<pill_editor_core::editor_state::EditorSelection>()
                .and_then(|selection| selection.entity);
            let transform = selected.and_then(|entity| {
                read_transform(world, entity).map(|transform| (entity, transform))
            });
            match transform {
                Some((entity, transform)) => (settings, entity, transform),
                None => return,
            }
        };
        let origin = transform.translation;
        let basis = self.gizmo_basis(settings.space, &transform);
        let length = self.gizmo_length(origin);
        let geometry = gizmo::build_geometry(settings.mode, origin, basis, length);
        let anchor = self.project_to_screen(origin);
        let world = self.host.host_mut().engine_mut().world_mut();
        if world
            .get_resource::<pill_engine_core::debug_draw::DebugDraw>()
            .is_none()
        {
            world.insert_resource(pill_engine_core::debug_draw::DebugDraw::new(true));
        }
        if let Some(debug) = world.get_resource_mut::<pill_engine_core::debug_draw::DebugDraw>() {
            for (start, end, color, handle) in &geometry.lines {
                debug.gizmo_line(*start, *end, *color, *handle);
            }
            for (corners, color, handle) in &geometry.triangles {
                debug.gizmo_triangle(*corners, *color, *handle);
            }
        }
        // The anchor line, on a change or on the periodic cadence: what the
        // suite (and a developer) reads to know where the handles are.
        if let Some((x, y)) = anchor {
            let header = (settings.mode, settings.space, x as i32, y as i32);
            if self.gizmo_logged != Some(header)
                || self.frame_count.is_multiple_of(GIZMO_HEARTBEAT_FRAMES)
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

    /// Publish the labels the overlay draws over the scene (Stage 21).
    ///
    /// The executable is the side that can project a world position into the
    /// scene view, so it hands the shell a client-area position and the text;
    /// the shell paints it in the overlay layer, which is the only layer that
    /// can sit above the scene view's own window.
    fn publish_overlay_labels(&mut self) {
        self.overlay_labels.clear();
        let entity = self
            .host
            .host_mut()
            .engine_mut()
            .world()
            .get_resource::<pill_editor_core::editor_state::EditorSelection>()
            .and_then(|selection| selection.entity);
        let Some(entity) = entity else {
            return;
        };
        let origin = {
            let world = self.host.host_mut().engine_mut().world_mut();
            match read_transform(world, entity) {
                Some(transform) => transform.translation,
                None => return,
            }
        };
        let Some((x, y)) = self.project_to_screen(origin) else {
            return;
        };
        // The projected point is in the scene surface's own physical pixels;
        // the overlay is drawn in the client area's logical ones.
        let scale_factor = self.window.scale_factor() as f32;
        let (scene_x, scene_y) = self
            .last_scene_rect
            .map(|rect| (rect.x, rect.y))
            .unwrap_or((0.0, 0.0));
        self.overlay_labels.push(EditorOverlayLabel {
            text: format!("entity {entity}"),
            position: Vector2f::new(
                scene_x + x / scale_factor,
                scene_y + y / scale_factor - OVERLAY_LABEL_LIFT,
            ),
        });
    }

    /// Write a changed gizmo choice to the project's preferences entry.
    ///
    /// The executable owns this write because it is the side that knows the
    /// project; the shell's commands only queue the change. The file coming
    /// back through the shell's live reader is what keeps the shell's mirror
    /// honest.
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
        let directory = pill_editor_core::layout::layout_directory();
        match pill_editor_core::commands::write_gizmo_preferences(&directory, &project, &settings) {
            Ok(()) => info!(
                target: telemetry_target::ENGINE,
                project = project.as_str(),
                mode = gizmo_mode_name(settings.mode),
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

    /// The frame the gizmo's axes are measured in.
    fn gizmo_basis(&self, space: GizmoSpace, transform: &TransformComponent) -> [[f32; 3]; 3] {
        if space == GizmoSpace::World {
            return gizmo::AXES;
        }
        [
            gizmo::rotate_vector(transform.rotation, gizmo::AXES[0]),
            gizmo::rotate_vector(transform.rotation, gizmo::AXES[1]),
            gizmo::rotate_vector(transform.rotation, gizmo::AXES[2]),
        ]
    }

    /// How long the handles are, in world units, at `origin`: a constant
    /// fraction of the camera distance, so the gizmo neither shrinks away
    /// nor swallows the scene.
    fn gizmo_length(&self, origin: [f32; 3]) -> f32 {
        (gizmo::distance(self.camera.eye(), origin) * GIZMO_SCREEN_FRACTION).clamp(0.05, 50.0)
    }

    /// The scene surface's size in physical pixels: the live window size,
    /// which the surface was attached and resized with. The last placed
    /// rectangle is only a fallback for a window that reports none yet.
    fn scene_pixel_size(&self) -> Option<(f32, f32)> {
        let size = self.scene.window.inner_size();
        if size.width > 0 && size.height > 0 {
            return Some((size.width as f32, size.height as f32));
        }
        let (_, _, width, height) = self.scene.placed?;
        Some((width as f32, height as f32))
    }

    /// The cursor ray through one physical pixel of the scene surface.
    ///
    /// Built from the editor camera's basis and the contract camera's
    /// vertical field of view - the same numbers the renderer projects with,
    /// so a pick and a drag agree about what a pixel looks at. `None` when
    /// the surface has no usable size yet.
    fn scene_ray(&self, x: f32, y: f32) -> Option<([f32; 3], [f32; 3])> {
        let (width, height) = self.scene_pixel_size()?;
        let (tan, aspect) = self.projection_terms(width, height);
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

    /// Where a world point lands on the scene surface, in physical pixels.
    fn project_to_screen(&self, point: [f32; 3]) -> Option<(f32, f32)> {
        let (width, height) = self.scene_pixel_size()?;
        let (tan, aspect) = self.projection_terms(width, height);
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

    /// The tangent of half the vertical field of view, and the aspect ratio.
    fn projection_terms(&self, width: f32, height: f32) -> (f32, f32) {
        let fov = CameraComponent::default().vertical_fov.clamp(1.0, 179.0);
        let tan = (fov.to_radians() * 0.5).tan();
        (tan, (width / height).max(1e-3))
    }

    /// Press on one axis handle of the active tool (the test channel, Stage
    /// 20): the press pixel is the handle tip's own, so the pick that answers
    /// it is proving the same geometry the renderer draws.
    fn start_synthetic_gizmo_drag(&mut self, axis: usize, delta_x: f32, delta_y: f32) {
        if self.gizmo_drag.is_some() {
            warn!(
                target: telemetry_target::ENGINE,
                "[editor] gizmo drag refused: one is already in flight"
            );
            return;
        }
        let world = self.host.host_mut().engine_mut().world_mut();
        let settings = world
            .get_resource::<GizmoState>()
            .map(|state| state.settings)
            .unwrap_or_default();
        let selected = world
            .get_resource::<pill_editor_core::editor_state::EditorSelection>()
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
        let basis = self.gizmo_basis(settings.space, &transform);
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
        let Some((x, y)) = self.project_to_screen(tip) else {
            warn!(
                target: telemetry_target::ENGINE,
                "[editor] gizmo drag refused: the handle is behind the camera"
            );
            return;
        };
        self.synthetic_gizmo_pending = Some((delta_x, delta_y));
        info!(
            target: telemetry_target::ENGINE,
            axis = gizmo_axis_name(axis),
            x,
            y,
            "[editor] gizmo drag press"
        );
        // The press pixel projects onto the handle's own geometry, but a
        // one-pixel line straddles pixel rows and a truncated coordinate
        // can land on the row next to it: the press asks the exact pixel
        // and its four neighbours, and the first that names a handle wins.
        let (centre_x, centre_y) = (x.round() as i64, y.round() as i64);
        self.synthetic_press_ids.clear();
        for (pixel_x, pixel_y) in [
            (centre_x, centre_y),
            (centre_x - 1, centre_y),
            (centre_x + 1, centre_y),
            (centre_x, centre_y - 1),
            (centre_x, centre_y + 1),
        ] {
            if let Some(id) = self.queue_scene_pick(pixel_x.max(0) as u32, pixel_y.max(0) as u32) {
                self.synthetic_press_ids.push(id);
            }
        }
    }

    /// Begin a drag from a handle pick: the press pixel's ray records the
    /// parameter or angle every later move is measured against.
    fn begin_gizmo_drag(&mut self, handle: u32, press_x: f32, press_y: f32) {
        let Some((mode, axis)) = gizmo::handle_axis(handle) else {
            warn!(
                target: telemetry_target::ENGINE,
                handle,
                "[editor] gizmo drag refused: unknown handle"
            );
            return;
        };
        let world = self.host.host_mut().engine_mut().world_mut();
        let settings = world
            .get_resource::<GizmoState>()
            .map(|state| state.settings)
            .unwrap_or_default();
        let selected = world
            .get_resource::<pill_editor_core::editor_state::EditorSelection>()
            .and_then(|selection| selection.entity);
        let Some(entity) = selected else {
            return;
        };
        let Some(original) = read_transform(world, entity) else {
            return;
        };
        let Some((ray_origin, ray_direction)) = self.scene_ray(press_x, press_y) else {
            return;
        };
        let basis = self.gizmo_basis(settings.space, &original);
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
        self.gizmo_drag_seq += 1;
        let reference = self.gizmo_length(origin);
        self.gizmo_drag = Some(GizmoDrag {
            mode,
            axis,
            entity,
            original,
            start_value,
            reference,
            coalesce_key: format!("gizmo.drag.{}", self.gizmo_drag_seq),
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
    /// against the press, snap it when the tool snaps, and queue the recorded
    /// batch every frame of the drag coalesces into.
    fn update_gizmo_drag(&mut self, cursor_x: f32, cursor_y: f32) {
        let Some(drag) = self.gizmo_drag.as_ref() else {
            return;
        };
        let (mode, axis) = (drag.mode, drag.axis);
        let entity = drag.entity;
        let reference = drag.reference;
        let start_value = drag.start_value;
        let original = drag.original;
        let coalesce_key = drag.coalesce_key.clone();
        let Some((ray_origin, ray_direction)) = self.scene_ray(cursor_x, cursor_y) else {
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
        let basis = self.gizmo_basis(settings.space, &original);
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
        let (commands, value) =
            gizmo_drag_commands(mode, axis, entity, &original, delta, &settings, reference);
        if commands.is_empty() {
            return;
        }
        let world = self.host.host_mut().engine_mut().world_mut();
        if world
            .get_resource::<pill_editor_core::commands::PendingEditorCommands>()
            .is_none()
        {
            world.insert_resource(pill_editor_core::commands::PendingEditorCommands::default());
        }
        if let Some(pending) =
            world.get_resource_mut::<pill_editor_core::commands::PendingEditorCommands>()
        {
            pending.push_recorded(
                commands,
                format!("gizmo {}", gizmo_mode_name(mode)),
                Some(coalesce_key),
            );
        }
        if let Some(drag) = self.gizmo_drag.as_mut() {
            drag.last_value = Some(value);
        }
    }

    /// End the drag in flight, reporting what it did; the coalesced batch it
    /// queued is the single undo step.
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

    /// Open a reported file in the developer's editor (Stage 19).
    ///
    /// `code -g <file>:<line>` is the default; `PILL_EDITOR_OPEN_COMMAND`
    /// replaces the program (its extra words become leading arguments),
    /// which the suite points at a recorder to see what a row asked for. The
    /// path is made absolute against the workspace root, the way the compiler
    /// that reported it resolved it.
    fn open_file_in_editor(&self, request: &EditorOpenRequest) {
        let path = std::path::Path::new(&request.file);
        let file = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.host.host().workspace_root().join(path)
        };
        let file = file.to_string_lossy().into_owned();
        info!(
            target: telemetry_target::ENGINE,
            file = file.as_str(),
            line = request.line,
            "[editor] open file"
        );
        let configured =
            std::env::var("PILL_EDITOR_OPEN_COMMAND").unwrap_or_else(|_| "code".to_string());
        let mut words = configured.split_whitespace();
        let program = words.next().unwrap_or("code").to_string();
        let spawned = std::process::Command::new(&program)
            .args(words)
            .arg("-g")
            .arg(format!("{file}:{line}", line = request.line))
            .spawn();
        if let Err(error) = spawned {
            let message = error.to_string();
            warn!(
                target: telemetry_target::ENGINE,
                program = program.as_str(),
                error = message.as_str(),
                "[editor] open file failed"
            );
        }
    }

    /// A held asset row dragged past the window edge asks for an OS drag
    /// (Stage 18); inside the window the move is just UI input. The drag
    /// itself starts at the next frame boundary, not here (see
    /// [`Self::drag_request`]).
    fn drag_out_if_past_the_edge(&mut self, position: &winit::dpi::PhysicalPosition<f64>) {
        if !self.mouse_left_held {
            return;
        }
        let Some(source) = self.drag_candidate.clone() else {
            return;
        };
        let width = self.configuration.width as f64;
        let height = self.configuration.height as f64;
        if position.x < 0.0 || position.y < 0.0 || position.x > width || position.y > height {
            self.drag_request = Some(source);
            self.drag_candidate = None;
        }
    }

    /// Drag one project file out to the OS (Stage 18): the shell reports an
    /// asset row held down, and a drag that leaves the window hands the file
    /// to the system as `CF_HDROP` (D28). Blocks in OLE's modal drag loop
    /// until the drag ends; frames resume after.
    fn start_drag_out(&mut self, source: &str) {
        let absolute = match &self.asset_root {
            Some(root) if !std::path::Path::new(source).is_absolute() => {
                std::path::Path::new(root).join(source)
            }
            _ => std::path::PathBuf::from(source),
        };
        info!(
            target: telemetry_target::ENGINE,
            source = %absolute.display(),
            "[editor] drag out began"
        );
        match platform::drag_out(&absolute) {
            Ok(platform::DragOutcome::Dropped) => info!(
                target: telemetry_target::ENGINE,
                outcome = "dropped",
                "[editor] drag out ended"
            ),
            Ok(platform::DragOutcome::Cancelled) => info!(
                target: telemetry_target::ENGINE,
                outcome = "cancelled",
                "[editor] drag out ended"
            ),
            Ok(platform::DragOutcome::Other(code)) => info!(
                target: telemetry_target::ENGINE,
                outcome = "other",
                code,
                "[editor] drag out ended"
            ),
            Err(error) => warn!(
                target: telemetry_target::ENGINE,
                outcome = "failed",
                error = %error,
                "[editor] drag out ended"
            ),
        }
        // The release that ended the gesture may have happened outside the
        // window, so the UI gets one here; otherwise the row would stay held
        // with no release ever arriving.
        self.pending_ui_events.push(InputEvent::MouseButton {
            button: MouseButton::Left,
            state: ButtonState::Released,
        });
        self.drag_candidate = None;
        self.mouse_left_held = false;
    }

    /// Aim the pointer at the Assets panel, for the scripted drag verbs; a
    /// real drag's pointer is wherever the user's cursor is.
    fn aim_at_assets(&mut self) {
        let position = self
            .last_assets_rect
            .map(|rect| Vector2f::new(rect.x + rect.width / 2.0, rect.y + rect.height / 2.0))
            .unwrap_or_else(|| {
                Vector2f::new(
                    self.configuration.width as f32 / 2.0,
                    self.configuration.height as f32 / 2.0,
                )
            });
        self.pending_ui_events
            .push(InputEvent::CursorMoved { position });
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
        // Step 2b: a drag the previous input asked for. OLE's modal loop runs
        // here, at the frame boundary, never inside winit's dispatch.
        if let Some(source) = self.drag_request.take() {
            self.start_drag_out(&source);
        }
        // Step 2c: the host's reload, patch and failure notices become the
        // shell's Problems rows and toasts (Stage 19). Published before the
        // frame so the shell sees them the moment the host produced them.
        self.publish_host_notices();
        // Step 2d: the selection's outline, published before the frame
        // renders through it (Stage 20); one resource write per change.
        self.publish_selection_outline();
        // Step 2e: the gizmo handles, rebuilt every frame so they follow the
        // entity, the camera and the tool.
        self.publish_gizmo_geometry();
        // Step 2f: a scripted gizmo drag (the test channel): press on the
        // named handle through the real pick, drag, release.
        if let Some((axis, dx, dy)) = self.synthetic_gizmo_drag.take() {
            self.start_synthetic_gizmo_drag(axis, dx, dy);
        }
        // Step 2g: the labels the overlay draws over the scene (Stage 21):
        // the executable projects them, the shell paints them.
        self.publish_overlay_labels();
        // Step 3: this frame's size, and the input queued since the last
        // frame, delivered to the shell module's input resource; the
        // executable's own state keeps its copy for the fallback.
        let scale_factor = self.window.scale_factor() as f32;
        let available = Vector2f::new(
            self.configuration.width as f32 / scale_factor,
            self.configuration.height as f32 / scale_factor,
        );
        // A monitor change (D30) keeps the shell's logical pixels and moves
        // the overlay's physical ones; the resize path does the same for the
        // client area itself.
        if let Some(overlay) = self.overlay.as_mut() {
            overlay.set_scale_factor(scale_factor);
        }
        // With no composition overlay, the list the last frame's shell built
        // is injected into the world's UI context here, the moment before the
        // renderer's frame builder drains it (Stage 21). The list is the same
        // one the overlay would have drawn, so the fallback shows the same
        // content.
        if self.overlay.is_none() {
            if let Some(list) = self.pending_overlay.take() {
                let world = self.host.host_mut().engine_mut().world_mut();
                if world.get_resource::<pill_ui::UiContext>().is_none() {
                    world.insert_resource(pill_ui::UiContext::new());
                }
                if let Some(ui) = world.get_resource_mut::<pill_ui::UiContext>() {
                    ui.inject(SCENE_SURFACE.0, scale_factor, list);
                }
            }
        }
        {
            // The Assets panel's rows come from the host's asset browser;
            // gathered before the engine borrow below. The tree walk touches
            // the filesystem, so it refreshes periodically rather than every
            // frame.
            if self.frame_count.is_multiple_of(ASSET_REFRESH_FRAMES) {
                self.asset_entries = self
                    .host
                    .host()
                    .asset_entries()
                    .into_iter()
                    .filter(|entry| !entry.is_directory)
                    .take(256)
                    .map(|entry| pill_editor_core::editor_state::AssetListing {
                        path: entry.path,
                        kind: entry.type_name.unwrap_or_else(|| "file".to_string()),
                    })
                    .collect();
                info!(
                    target: telemetry_target::ENGINE,
                    count = self.asset_entries.len(),
                    "[editor] asset listing refreshed"
                );
            }
            let systems_paused = self.host.host_mut().engine_mut().systems_paused();
            let engine = self.host.host_mut().engine_mut();
            // The editor snapshot the shell's panels draw from (Stage 16).
            // The cheap tiers rebuild each frame; the entity tier is one row
            // with owned strings per live entity and costs real time on a
            // big world (circus_demo's 200k entities measured ~0.7 s/frame
            // in debug), so it rebuilds on change - the entity count moved,
            // or an edit batch applied - with a periodic backstop for
            // component changes made off the command path.
            let live_entities = engine.world().entity_count();
            let refresh_entities = self.entity_tier_dirty
                || live_entities != self.entity_tier_count
                || self.entity_tier_age >= ENTITY_TIER_MAX_AGE_FRAMES;
            let entities = refresh_entities
                .then(|| pill_editor_core::editor_state::EditorSnapshot::capture_entities(engine));
            let systems =
                pill_editor_core::editor_state::EditorSnapshot::capture_systems(engine, &[]);
            let components =
                pill_editor_core::editor_state::EditorSnapshot::capture_components(engine);
            let selected = engine
                .world()
                .get_resource::<pill_editor_core::editor_state::EditorSelection>()
                .and_then(|selection| selection.entity);
            let has_snapshot = engine
                .world()
                .get_resource::<pill_editor_core::editor_state::EditorSnapshot>()
                .is_some();
            if has_snapshot {
                let detail = selected.and_then(|entity| {
                    pill_editor_core::editor_state::EditorSnapshot::capture_detail(engine, entity)
                });
                let world = engine.world_mut();
                if let Some(current) =
                    world.get_resource_mut::<pill_editor_core::editor_state::EditorSnapshot>()
                {
                    if let Some(entities) = entities {
                        current.entities = entities;
                    }
                    current.systems = systems;
                    current.components = components;
                    current.assets = self.asset_entries.clone();
                    current.paused = systems_paused;
                    current.detail = detail;
                }
            } else {
                // The first frame owns the full capture; later frames update
                // the existing resource in place.
                let mut snapshot = pill_editor_core::editor_state::EditorSnapshot::capture_list(
                    engine,
                    0,
                    &[],
                    Vec::new(),
                );
                snapshot.assets = self.asset_entries.clone();
                snapshot.detail = selected.and_then(|entity| {
                    pill_editor_core::editor_state::EditorSnapshot::capture_detail(engine, entity)
                });
                engine.world_mut().insert_resource(snapshot);
            }
            if refresh_entities {
                self.entity_tier_count = live_entities;
                self.entity_tier_age = 0;
                self.entity_tier_dirty = false;
            } else {
                self.entity_tier_age += 1;
            }
            let world = engine.world_mut();
            if let Some(input) = world.get_resource_mut::<EditorShellInput>() {
                input.events.append(&mut self.pending_ui_events);
                input.logical_size = available;
                input.scale_factor = scale_factor;
                input.editor_frames_per_second = self.frames_per_second;
                input.requested_tab = self.requested_tab.take();
                input.requested_panel = self.requested_panel.take();
                input.requested_widget = self.requested_widget.take();
                input.requested_wheel = self.requested_wheel.take();
                input
                    .requested_commands
                    .append(&mut self.requested_commands);
                input.hovered_files = self.hovered_files.clone();
                input.dropped_files = std::mem::take(&mut self.dropped_files);
                input.asset_root = self.asset_root.clone();
                input.systems_paused = systems_paused;
                // Stage 19: the problem list the panel draws, and the toasts
                // queued since the shell's last drain. The events are moved,
                // not copied; the problems are replaced (they only change on
                // a notice, which is rare).
                input.notices.problems.clone_from(&self.notices.problems);
                input.notices.revision = self.notices.revision;
                input.notices.next_event_seq = self.notices.next_event_seq;
                input.notices.events.append(&mut self.notices.events);
                // Stage 21: the popups' own input - a pointer inside a popup
                // window never arrives as an `InputEvent` - and the popups
                // the platform closed since the last frame, which the shell
                // clears its state from.
                input
                    .popup_pointers
                    .append(&mut self.popups.take_pointers());
                input.popup_closed.append(&mut self.popup_closed);
                input.overlay_labels.clone_from(&self.overlay_labels);
                // A shell that never drains (the fallback UI, or a module
                // that failed to load) must not accumulate forever.
                let excess = input.notices.events.len().saturating_sub(TOAST_QUEUE_MAX);
                input.notices.events.drain(..excess);
            } else {
                // No shell module: the fallback already consumed the events.
                self.pending_ui_events.clear();
                // Nothing reads the popups' pointers either, so they must not
                // accumulate across frames.
                self.popups.take_pointers();
                self.popup_closed.clear();
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
                    objects: std::mem::take(&mut frame.objects),
                    clipboard_read: frame.requested_clipboard.take(),
                    assets_rect: frame.assets_rect,
                    drag_candidate: frame.drag_candidate.take(),
                    requested_open: frame.requested_open.take(),
                    scene_rect: frame.scene_rect,
                    game_rect: frame.game_rect,
                    overlay: std::mem::take(&mut frame.overlay),
                    overlay_opaque: std::mem::take(&mut frame.overlay_opaque),
                    popups: std::mem::take(&mut frame.popups),
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
                    objects: Vec::new(),
                    clipboard_read: None,
                    assets_rect: self.last_assets_rect,
                    drag_candidate: None,
                    requested_open: None,
                    scene_rect: self.last_scene_rect,
                    game_rect: self.last_game_rect,
                    overlay: DrawList::default(),
                    overlay_opaque: Vec::new(),
                    popups: Vec::new(),
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
            if !batches.is_empty() {
                // An applied edit can change archetypes (add/remove
                // component), so the snapshot's entity tier is stale.
                self.entity_tier_dirty = true;
            }
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
        self.last_assets_rect = output.assets_rect;
        self.drag_candidate = output.drag_candidate;
        // Stage 21: what floats over the scene, and where it is opaque. The
        // overlay list goes to the composition surface at the end of this
        // frame, or into the renderer's UI pass when there is none; the
        // popups become windows at the next frame boundary; and a click over
        // an opaque rectangle is the UI's, not the scene's.
        self.overlay_opaque = output.overlay_opaque;
        self.requested_popups = Some(output.popups);
        let overlay_list = output.overlay;
        let overlay_vertices = overlay_list.vertices.len();
        if self.overlay.is_some() {
            // Drawn below, after the scene has presented.
            self.pending_overlay = Some(overlay_list);
        } else if !overlay_list.is_empty() {
            // No composition overlay: the same list is folded into the
            // renderer's UI pass. It goes to the world's UI context, which
            // the renderer's frame builder drains at the start of the next
            // frame, and the shell's atlas additions go with it, so the
            // list's texture coordinates find the same glyphs.
            let world = self.host.host_mut().engine_mut().world_mut();
            if world.get_resource::<pill_ui::UiContext>().is_none() {
                world.insert_resource(pill_ui::UiContext::new());
            }
            if let Some(update) = output.atlas.as_ref() {
                if let Some(ui) = world.get_resource_mut::<pill_ui::UiContext>() {
                    ui.adopt_atlas(update);
                }
            }
            self.pending_overlay = Some(overlay_list);
        }
        // The overlay's own heartbeat: which path the UI floats on, and how
        // much it carries. Logged when the content changes or on the periodic
        // cadence, so a suite can tell the two paths apart from the log.
        if overlay_vertices != self.overlay_vertices_logged
            || self.frame_count.is_multiple_of(OVERLAY_HEARTBEAT_FRAMES)
        {
            self.overlay_vertices_logged = overlay_vertices;
            info!(
                target: telemetry_target::ENGINE,
                path = if self.overlay.is_some() {
                    "composition"
                } else {
                    "scene-pass"
                },
                forced = self.overlay_forced_failure,
                frames = self.overlay.as_ref().map(OverlaySurface::frames).unwrap_or(0),
                vertices = overlay_vertices,
                opaque = self.overlay_opaque.len(),
                popups = self.popups.len(),
                opened = self.popups.opened(),
                labels = self.overlay_labels.len(),
                "[editor] overlay content"
            );
        }
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
        // Step 7: clipboard traffic (D28). Widget requests and the shell's
        // structured objects are written through the platform layer; a
        // payload read the shell asked for lands in `EditorClipboardAnswer`
        // for a later frame, so a slow clipboard owner never stalls here.
        for request in &output.clipboard {
            match request {
                ClipboardRequest::SetText(text) => match platform::set_text(text) {
                    Ok(()) => info!(
                        target: telemetry_target::ENGINE,
                        len = text.len(),
                        "[editor] clipboard set text"
                    ),
                    Err(error) => warn!(
                        target: telemetry_target::ENGINE,
                        error = error.as_str(),
                        "[editor] clipboard write failed"
                    ),
                },
                ClipboardRequest::GetText => match platform::text() {
                    Some(text) => {
                        info!(
                            target: telemetry_target::ENGINE,
                            len = text.len(),
                            "[editor] clipboard read text"
                        );
                        // The paste lands as typed text in the focused field
                        // next frame, the same path `type` takes.
                        self.pending_ui_events.push(InputEvent::TextInput { text });
                    }
                    None => info!(
                        target: telemetry_target::ENGINE,
                        len = 0,
                        "[editor] clipboard read text"
                    ),
                },
            }
        }
        for object in &output.objects {
            match platform::set_object(&object.format, &object.bytes, &object.text) {
                Ok(()) => info!(
                    target: telemetry_target::ENGINE,
                    format = object.format.as_str(),
                    bytes = object.bytes.len(),
                    "[editor] clipboard wrote object"
                ),
                Err(error) => warn!(
                    target: telemetry_target::ENGINE,
                    format = object.format.as_str(),
                    error = error.as_str(),
                    "[editor] clipboard object write failed"
                ),
            }
        }
        if let Some(format) = output.clipboard_read {
            self.clipboard_seq += 1;
            let bytes = platform::object(&format);
            let text = platform::text();
            info!(
                target: telemetry_target::ENGINE,
                format = format.as_str(),
                bytes = bytes.as_ref().map_or(0, Vec::len),
                text = text.as_ref().map_or(0, String::len),
                "[editor] clipboard read"
            );
            let answer = pill_editor_api::EditorClipboardAnswer {
                format,
                bytes,
                text,
                seq: self.clipboard_seq,
            };
            let world = self.host.host_mut().engine_mut().world_mut();
            match world.get_resource_mut::<pill_editor_api::EditorClipboardAnswer>() {
                Some(current) => *current = answer,
                None => world.insert_resource(answer),
            }
        }
        // Step 4c: a Problems row asked for a file (Stage 19): hand it to the
        // developer's editor, out of band.
        if let Some(request) = output.requested_open {
            self.open_file_in_editor(&request);
        }
        // Step 4d: answers to scene picks (Stage 20) arrived during the
        // frame's render; the selection they name is applied here.
        self.drain_scene_picks();
        // Step 4e: a gizmo choice the shell's commands changed is written to
        // the project's preferences entry, once, from the side that knows
        // the project.
        self.persist_gizmo_settings();
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
        // Step 8b: the overlay and the popups present their own surfaces
        // (Stage 21). The overlay draws after the scene has presented, so its
        // content lands on the newest scene image; the popups are separate
        // windows entirely.
        if let Some(overlay) = self.overlay.as_mut() {
            if let Some(list) = self.pending_overlay.take() {
                if let Err(error) = overlay.render(&self.device, &self.queue, &mut self.ui, &list) {
                    warn!(
                        target: telemetry_target::ENGINE,
                        error = error.as_str(),
                        "[editor] overlay frame skipped"
                    );
                }
            }
        }
        self.popups
            .render(&self.device, &self.queue, &mut self.ui, self.theme.window);
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
    /// Widget clipboard requests to fulfil (text copy, paste reads).
    clipboard: Vec<ClipboardRequest>,
    /// Structured payloads to offer the system clipboard.
    objects: Vec<pill_editor_api::ClipboardObject>,
    /// A payload the shell asked the executable to read, by format name.
    clipboard_read: Option<String>,
    /// Where the Assets panel sits, for aiming a scripted file drop.
    assets_rect: Option<EditorRect>,
    /// An asset row held down, for an OS drag when the pointer leaves the
    /// window (Stage 18).
    drag_candidate: Option<String>,
    /// A file the shell asked the executable to open, from a Problems row
    /// (Stage 19).
    requested_open: Option<EditorOpenRequest>,
    /// Where the scene surface goes, in logical pixels.
    scene_rect: Option<EditorRect>,
    /// Where the game surface goes, in logical pixels.
    game_rect: Option<EditorRect>,
    /// The UI that floats above the scene surfaces (Stage 21, D24): the FPS
    /// badge, the toasts, the docking and file-drop previews, the entity
    /// labels. Drawn through the composition overlay when there is one, and
    /// through the renderer's UI pass when there is not.
    overlay: DrawList,
    /// The overlay's opaque rectangles, in logical client pixels: input over
    /// one of them belongs to the UI, everywhere else to the scene beneath.
    overlay_opaque: Vec<EditorRect>,
    /// The popups the shell wants open (Stage 21, D23).
    popups: Vec<EditorPopup>,
}

// =============================================================================
// Host notices to editor notices (Stage 19)
// =============================================================================

/// The toast a host notice becomes: its severity and its one line.
fn notice_toast(notice: &HostNotice) -> (EditorProblemLevel, String) {
    match notice {
        HostNotice::ModuleFailed { module, .. } => (
            EditorProblemLevel::Error,
            format!("{module}: reload failed; the previous generation still runs"),
        ),
        HostNotice::Reloaded { module, build_ms } => {
            let text = if *build_ms == 0 {
                format!("{module} reloaded")
            } else {
                format!("{module} reloaded in {build_ms} ms")
            };
            (EditorProblemLevel::Note, text)
        }
        HostNotice::PatchApplied {
            function,
            milliseconds,
        } => (
            EditorProblemLevel::Note,
            format!("{function} patched live in {milliseconds:.0} ms"),
        ),
        HostNotice::PatchFailed { function, detail } => (
            EditorProblemLevel::Error,
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
            (EditorProblemLevel::Warning, text)
        }
    }
}

/// The wire name of a toast level, for the log line the suites match.
fn notice_level_name(level: EditorProblemLevel) -> &'static str {
    match level {
        EditorProblemLevel::Error => "error",
        EditorProblemLevel::Warning => "warning",
        EditorProblemLevel::Note => "info",
    }
}

/// The Problems rows a module's failed output becomes.
///
/// The build text is parsed for compiler diagnostics; when none is
/// recognisable - a linker error, a foreign toolchain's wording - the first
/// non-empty line is kept as one row, so a failure never shows as nothing.
fn problems_from_build_output(module: &str, output: &str) -> Vec<EditorProblem> {
    let parsed = pill_editor_core::problems::parse_build_output(output);
    if parsed.is_empty() {
        let message = output
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("build failed")
            .to_string();
        return vec![EditorProblem {
            module: module.to_string(),
            level: EditorProblemLevel::Error,
            message,
            file: None,
            line: None,
            column: None,
        }];
    }
    parsed
        .into_iter()
        .map(|problem| EditorProblem {
            module: module.to_string(),
            level: match problem.severity {
                pill_editor_core::problems::Severity::Error => EditorProblemLevel::Error,
                pill_editor_core::problems::Severity::Warning => EditorProblemLevel::Warning,
                pill_editor_core::problems::Severity::Note => EditorProblemLevel::Note,
            },
            message: problem.message,
            file: problem.file,
            line: problem.line,
            column: problem.column,
        })
        .collect()
}

// =============================================================================
// Gizmo drag (Stage 20)
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

/// Read one entity's transform as plain arrays; `None` when the entity is
/// not alive, carries no transform, or a field does not read back as floats.
fn read_transform(
    world: &pill_engine_core::World,
    entity: pill_engine_core::Entity,
) -> Option<TransformComponent> {
    use pill_engine_core::FieldValue;
    let name = pill_engine_core::common_components::TRANSFORM_SHARED_NAME;
    let read = |field: &str| -> Option<Vec<f32>> {
        let value = world.read_component_field(entity, name, field).ok()?;
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

/// The commands one gizmo drag move produces, and the values it writes (for
/// the log).
///
/// Pure and renderer-free, so the drop target of the drag path - a batch of
/// the same recorded commands a panel edit issues - is testable headlessly:
/// the unit test drives this as a stub renderer's pick answer would, the
/// suite drives the real one. `reference` is the handles' world length at
/// the press, which is what a scale drag's delta is measured against.
fn gizmo_drag_commands(
    mode: GizmoMode,
    axis: usize,
    entity: pill_engine_core::Entity,
    original: &TransformComponent,
    delta: f32,
    settings: &GizmoSettings,
    reference: f32,
) -> (Vec<EditorCommand>, [f32; 4]) {
    use pill_engine_core::FieldValue;
    let component = pill_engine_core::common_components::TRANSFORM_SHARED_NAME.to_string();
    match mode {
        GizmoMode::Translate => {
            let delta = if settings.snap {
                gizmo::snap_value(delta, settings.translate_step)
            } else {
                delta
            };
            let value = original.translation[axis] + delta;
            let batch = vec![EditorCommand::SetArrayElement {
                entity,
                component,
                field: "translation".to_string(),
                index: axis,
                value: FieldValue::F32(value),
            }];
            (batch, [value, 0.0, 0.0, 0.0])
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
            let batch = (0..4)
                .map(|index| EditorCommand::SetArrayElement {
                    entity,
                    component: component.clone(),
                    field: "rotation".to_string(),
                    index,
                    value: FieldValue::F32(combined[index]),
                })
                .collect();
            (batch, combined)
        }
        GizmoMode::Scale => {
            let factor = 1.0 + delta / reference.max(1e-4);
            let mut value = original.scale[axis] * factor;
            if settings.snap {
                value = gizmo::snap_value(value, settings.scale_step);
            }
            // A zero or negative scale is a degenerate mesh; the drag stops
            // at the smallest positive value instead.
            let value = value.max(0.01);
            let batch = vec![EditorCommand::SetArrayElement {
                entity,
                component,
                field: "scale".to_string(),
                index: axis,
                value: FieldValue::F32(value),
            }];
            (batch, [value, 0.0, 0.0, 0.0])
        }
    }
}

/// One gizmo drag in flight.
///
/// The press records where the drag started - the axis parameter or ring
/// angle the cursor ray named, plus the transform it started from - so every
/// move applies a delta against it and nothing jumps on the first pixel.
struct GizmoDrag {
    /// The tool mode the grabbed handle belongs to.
    mode: GizmoMode,
    /// The axis index the handle belongs to.
    axis: usize,
    /// The entity being dragged.
    entity: pill_engine_core::Entity,
    /// The transform at the press; every move is computed from it, so a
    /// snapped drag cannot accumulate rounding.
    original: TransformComponent,
    /// The axis parameter (translate, scale) or ring angle (rotate) at the
    /// press.
    start_value: f32,
    /// How long the handles were at the press, in world units: the scale
    /// drag's reference.
    reference: f32,
    /// The coalesce key this drag's edits share, so the whole drag is one
    /// undo step (the same rule a slider drag follows).
    coalesce_key: String,
    /// The value written on the previous move, for the end log.
    last_value: Option<[f32; 4]>,
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
    /// Whether a placement has been logged yet. The first one always
    /// reports, so a view the layout starts shown does not go unlogged -
    /// the suite reads the view's rectangle from that line.
    reported: bool,
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
                        reported: false,
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
            reported: false,
            orbiting: false,
            panning: false,
            last_cursor: None,
        })
    }

    /// Show or hide the window; returns whether the log should record the
    /// placement: on a real change, and on the first call so every view's
    /// starting state is logged once.
    fn set_shown(&mut self, visible: bool) -> bool {
        let changed = self.visible != visible;
        if changed {
            self.visible = visible;
            self.window.set_visible(visible);
            if !visible {
                self.orbiting = false;
                self.panning = false;
                self.last_cursor = None;
            }
        }
        let first = !self.reported;
        self.reported = true;
        changed || first
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

/// Frames between asset-browser refreshes (the walk touches the disk).
const ASSET_REFRESH_FRAMES: u64 = 120;

/// Backstop cadence for the snapshot's entity tier: even with no count
/// change and no applied edit, it refreshes this often, because a system
/// can add or remove a component without going through a command.
const ENTITY_TIER_MAX_AGE_FRAMES: u32 = 600;

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

/// The suite's containment fixture panel: it always panics, so the shell's
/// containment path is exercised end to end.
fn containment_test_panel(
    _builder: &mut pill_ui::PanelBuilder<'_>,
    _context: &mut pill_editor_api::panels::PanelContext<'_>,
) {
    panic!("containment test panel");
}

/// Parse a scripted field value: `true`/`false`, a float (one with a `.`
/// or an `f` suffix), or an integer (`u` suffix for an unsigned one).
fn parse_field_value(token: &str) -> Result<pill_engine_core::FieldValue, String> {
    use pill_engine_core::FieldValue;
    match token {
        "true" => return Ok(FieldValue::Bool(true)),
        "false" => return Ok(FieldValue::Bool(false)),
        _ => {}
    }
    if let Some(digits) = token.strip_suffix('u') {
        if let Ok(value) = digits.parse::<u32>() {
            return Ok(FieldValue::U32(value));
        }
    }
    if token.contains('.') || token.ends_with('f') {
        let digits = token.strip_suffix('f').unwrap_or(token);
        if let Ok(value) = digits.parse::<f32>() {
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
