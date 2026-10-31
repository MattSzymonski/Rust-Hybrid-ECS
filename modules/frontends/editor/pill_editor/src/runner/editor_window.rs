//! The editor window's state: its fields and its constructor.
//!
//! # Responsibilities
//!
//! - Hold everything one editor frame reads and writes: the window and its
//!   device, the host, the scene views and popouts, the shell session, the
//!   cameras, the gizmo drag in flight and the caches the frame keeps.
//! - Build it once, in [`EditorWindow::open`], which creates the overlay, the
//!   scene views and the first shell module.
//!
//! # Design
//!
//! The state is one struct because the frame's phases run on one borrow of it;
//! the methods that use it live in the sibling modules, one per concern. The
//! fields are `pub(super)` so those modules can read them - the alternative, a
//! getter per field, would be a hundred wrappers around plain data.

// Current crate
use super::*;

/// The editor's window, its host, and everything the frame loop owns.
pub(super) struct EditorWindow {
    /// The main window: the shell's UI, and the game view's primary surface
    /// only when the game window could not be created.
    pub(super) window: Arc<Window>,
    /// The editor's own GPU device, which draws the UI; the project's
    /// renderer keeps its own device inside the host.
    pub(super) device: wgpu::Device,
    /// The queue of the editor's device.
    pub(super) queue: wgpu::Queue,
    /// The editor's instance, kept for late overlay creation.
    pub(super) instance: wgpu::Instance,
    /// The adapter the editor's device runs on, kept for late overlay
    /// creation.
    pub(super) adapter: wgpu::Adapter,
    /// The host: the project, its extensions and the shell module.
    pub(super) host: RenderingHost,
    /// The scene views' child windows, one per scene tab, in view-index
    /// order: each has its own camera and renderer surface. A slot is `None`
    /// when its window could not be created, and the vector grows when the
    /// dock reports a view it does not hold yet.
    pub(super) views: Vec<Option<SceneView>>,
    /// The game viewport's window, when one could be created: it carries
    /// the primary surface, so the project's renderer presents the game
    /// view there. `None` leaves the primary on the main window.
    pub(super) game_surface: Option<SurfaceWindow>,
    /// One window per popped-out tab, keyed by the tab's
    /// wire name: each carries its own swapchain and its own egui renderer,
    /// because the UI that draws into it is a pass of its own.
    pub(super) popouts: std::collections::BTreeMap<String, PopoutWindow>,
    /// The editor's cameras, one per scene viewport: navigation moves the
    /// active viewport's camera only, and every scene ray projects through
    /// its own view's camera.
    pub(super) cameras: Vec<OrbitCamera>,
    /// The viewport the last scene click activated; camera navigation and
    /// the gizmo read it.
    pub(super) active_view: usize,
    /// Whether each scene view's left button is down: a pick answers a frame
    /// or two after the press, and a handle grab only starts a drag while
    /// the button is still held.
    pub(super) left_down: Vec<bool>,
    /// The camera navigation drag in flight, when one is.
    pub(super) camera_drag: Option<CameraDrag>,
    /// The keyboard modifiers held, as last reported by a modifiers event.
    pub(super) modifiers: Modifiers,
    /// The keymap preset the preferences name: the navigation table in force.
    pub(super) keymap: KeymapPreset,
    /// Whether each view's published camera binding is stale: navigation
    /// marks it and the next frame re-publishes the surface camera.
    pub(super) camera_dirty: Vec<bool>,
    /// egui's own renderer on the editor's device.
    pub(super) egui_renderer: egui_wgpu::Renderer,
    /// The composition overlay the UI is presented through; `None` until it
    /// is created, and left `None` when the platform refuses it.
    pub(super) overlay: Option<OverlaySurface>,
    /// `PILL_EDITOR_NO_OVERLAY` asked for the window-surface posture, so the
    /// composition overlay is never retried.
    pub(super) no_overlay: bool,
    /// Frames between overlay creation attempts after a failure.
    pub(super) overlay_retry_frame: u64,
    /// The editor's fallback context, used while no shell module is loaded.
    pub(super) fallback_context: egui::Context,
    /// Whether the fallback UI was reported already.
    pub(super) reported_fallback: bool,
    /// The window events not yet pushed into the engine's input.
    pub(super) pending_events: Vec<InputEvent>,
    /// The editor's own windows' events, not yet handed to the shell.
    pub(super) shell_events: Vec<InputEvent>,
    /// The pointer's last position over the editor's windows, in the main
    /// window's client pixels; the shell input carries it so a release that
    /// arrives without a move still has a place to land.
    pub(super) shell_cursor: Option<pill_core::math::Vector2f>,
    /// The notices the host's queue produced: the toast events and the
    /// Problems rows the shell reads.
    pub(super) notices: EditorNotices,
    /// The last pointer position each scene window reported, in its own
    /// client-area physical pixels; `None` before its first move.
    pub(super) scene_cursor: Vec<Option<(f32, f32)>>,
    /// The scene left press waiting for its pick answer: the view, the press
    /// position and the pick id. The answer starts a gizmo drag on a handle
    /// or moves the selection - both while the button is held.
    pub(super) press_pick: Option<PressPick>,
    /// The entities the renderer's outline set currently names, so the world
    /// resource is written once per selection change.
    pub(super) outlined_entities: Vec<Entity>,
    /// The overlay chips' rectangles the shell reported last frame, in
    /// logical points: clicks inside them are the UI's, not the scene's.
    pub(super) overlay_regions: Vec<ShellRect>,
    /// The gizmo drag in flight, when one is.
    pub(super) gizmo_drag: Option<GizmoDrag>,
    /// The gizmo geometry drawn this frame: the hover test runs against the
    /// exact handles on screen, so the highlight cannot disagree with them.
    pub(super) gizmo_geometry: GizmoGeometry,
    /// The handle under the pointer, when one is.
    pub(super) hovered_handle: Option<u32>,
    /// How long the handles were when they were last built, in world units:
    /// the hover test's radius and the drag guides' dash length follow it.
    pub(super) gizmo_reference: f32,
    /// The handle hit radius as a fraction of the handle length, from the
    /// preferences file: the hover test and the press share it.
    pub(super) gizmo_hit_radius: f32,
    /// Whether the preferences file asks for rendered asset previews, and the
    /// paths whose preview is on its way to the renderer.
    pub(super) render_previews: bool,
    pub(super) preview_in_flight: std::collections::HashMap<u64, String>,
    /// Whether the drag guides drew last frame, so the log line appears on
    /// the state change instead of every frame.
    pub(super) gizmo_guides: Option<bool>,
    /// The last logged gizmo anchor header (mode, space, x, y): the log
    /// line appears on changes and on the heartbeat.
    pub(super) gizmo_logged: Option<(GizmoMode, GizmoSpace, i32, i32)>,
    /// The pick ids a scripted gizmo grab pressed: the first answer that
    /// names a handle starts the drag, a full miss refuses it.
    pub(super) synthetic_press_ids: Vec<u64>,
    /// The pixel delta a scripted grab applies once its pick answers.
    pub(super) synthetic_gizmo_pending: Option<(f32, f32)>,
    /// Whether a scripted press holds the drag open instead of finishing it:
    /// `gizmo press`, `gizmo move` and `gizmo release` drive the same drag a
    /// real mouse does, one command per frame.
    pub(super) synthetic_gizmo_hold: bool,
    /// The press pixel of the scripted drag in flight, in its view's
    /// physical pixels: every scripted move measures its delta from here.
    pub(super) synthetic_gizmo_press: Option<(f32, f32)>,
    /// The view a scripted grab belongs to.
    pub(super) synthetic_gizmo_view: Option<usize>,
    /// The asset browser's rows, refreshed on a cadence and at once after
    /// any file command (the Assets tab draws them).
    pub(super) asset_entries: Vec<AssetListing>,
    /// The last walk's own rows, before the module roots' synthetic folders
    /// are added: what the next walk is compared against to tell whether the
    /// tree moved.
    pub(super) asset_walk: Vec<AssetListing>,
    /// The live standalone asset types the create dialog offers.
    pub(super) standalone_types: Vec<StandaloneTypeInfo>,
    /// Whether a file command changed the tree: the next frame's listing
    /// refresh walks it at once.
    pub(super) asset_listing_dirty: bool,
    /// The file a drag from the OS is carrying over the editor, if one is: the
    /// drop preview's subject.
    pub(super) drop_preview: Option<std::path::PathBuf>,
    /// The recovery document a previous run left behind,
    /// applied on the first frame the shell's session exists.
    pub(super) recovered: Option<pill_editor_core::layout::RecoveryDocument>,
    /// The state the last recovery write captured, so an unchanged editor
    /// does not rewrite the document every frame.
    pub(super) last_recovery_key: Option<String>,
    /// Whether the main window currently takes IME input,
    /// and the caret rectangle it was last given.
    pub(super) ime_allowed: bool,
    /// The IME candidate window's anchor, in logical points.
    pub(super) ime_rect: Option<(f32, f32, f32, f32)>,
    /// The selected asset's detail for the Inspector's asset section.
    pub(super) asset_detail: Option<AssetDetail>,
    /// The path `asset_detail` was captured for, so the capture follows the
    /// selection without a disk read every frame.
    pub(super) asset_detail_path: Option<String>,
    /// The preferences file: its text and stamp, read when the file moves. The
    /// live reader applies the history limits and the snapping steps when it
    /// changes.
    pub(super) preferences: StateFileWatch,
    /// The keybindings file: a change re-applies the preset and the overrides
    /// to the command registry.
    pub(super) keybindings: StateFileWatch,
    /// Whether the keybindings watcher has applied once: the first pass
    /// applies the editor's default preset even when there is no file, and
    /// until it has applied, the file is re-read every frame.
    pub(super) keybindings_applied: bool,
    /// The telemetry capture ring the Console tab reads.
    pub(super) log_buffer: pill_core::telemetry::capture::LogBuffer,
    /// The parsed preferences document the snapshot's view and the scripted
    /// dump build on.
    pub(super) preferences_document: serde_json::Map<String, serde_json::Value>,
    /// The theme the editor's UI paints with: the code
    /// default merged with the user's file and the project's layer.
    pub(super) theme: pill_ui_theme::Theme,
    /// How many themes the layers have resolved to; the shell installs a new
    /// one when this moves and leaves the current one alone when it does not.
    pub(super) theme_generation: u64,
    /// The layers [`Self::theme`] was merged from, in apply order.
    pub(super) theme_layers: Vec<ThemeLayerInfo>,
    /// The font bytes the layers resolved: the snapshot
    /// carries them beside the theme, so the shell installs both together.
    pub(super) theme_fonts: pill_editor_core::kit::style::ThemeFonts,
    /// What the last theme resolution read, so a poll only merges when a
    /// layer moved.
    pub(super) theme_watch: ThemeWatch,
    /// The generation the fallback context paints, so the no-shell posture
    /// follows a theme change the same way the shell does.
    pub(super) fallback_theme_generation: u64,
    /// The project directory name the per-project preferences are keyed by.
    pub(super) gizmo_project: Option<String>,
    /// The gizmo settings last written to the preferences file, so an
    /// unchanged tool writes nothing.
    pub(super) gizmo_persisted: Option<GizmoSettings>,
    /// Whether the main window currently has focus.
    pub(super) focused: bool,
    /// The main window's client size in physical pixels.
    pub(super) size: (u32, u32),
    /// The window's scale factor, as the shell's logical pixels run at.
    pub(super) scale_factor: f32,
    /// World frames run since startup.
    pub(super) frame_index: u64,
    /// When the last frame started, for the smoothed FPS.
    pub(super) last_frame_time: Option<Instant>,
    /// When the heartbeat last printed; the heartbeat fires every 210 frames
    /// or every two seconds, whichever comes first, so a slow run (the
    /// benchmark's debug posture) still reports inside its window.
    pub(super) last_heartbeat: Option<Instant>,
    /// The smoothed frame rate, logged with the heartbeat.
    pub(super) fps: f32,
    /// When the run started, against which the test deadline is measured.
    pub(super) started: Instant,
    /// Whether egui claimed the pointer in the last frame, logged on change
    /// so a suite can see the routing decision.
    pub(super) claimed_pointer: bool,
    /// Whether egui claimed the keyboard in the last frame.
    pub(super) claimed_keyboard: bool,
    /// The cursor egui asked for in the last frame, so a change is logged
    /// once instead of every frame: the resize cursors over a splitter are how
    /// the widened grab band shows itself.
    pub(super) last_cursor: egui::CursorIcon,
    /// The scripted command channel, when the environment names one.
    pub(super) test_channel: Option<TestChannel>,
    /// A scripted left-button drag in flight, when one is.
    pub(super) scripted_drag: Option<ScriptedDrag>,
    /// The scene title last reported, so the log names each state the
    /// document went through once instead of every frame.
    pub(super) scene_title: String,
    /// Quit after this many seconds, when the environment asks for it.
    pub(super) test_seconds: Option<f64>,
    /// A quit the scripted channel or the deadline asked for.
    pub(super) quit: bool,
    /// When the loop draws next.
    pub(super) cadence: Cadence,
    /// What has asked for the next frame since the last one ran.
    pub(super) demand: FrameDemand,
    /// The decision the last frame left for the loop.
    pub(super) next_wake: FrameWake,
    /// egui's own repaint request from the last shell pass.
    pub(super) repaint_after: Option<Duration>,
    /// What asked for the last frame, kept for the cadence dump.
    pub(super) last_demand: FrameDemand,
    /// The entity badges this frame's scene views draw:
    /// every visible view's entities, projected to logical points.
    pub(super) badges: Vec<ShellBadge>,
    /// Whether the last decision left the loop in the polled state, so the
    /// `[editor] frame idle` line appears once per transition.
    pub(super) polling: bool,
    /// The most expensive frame drawn while polling since the last
    /// heartbeat, in milliseconds: the measured cost of an idle frame.
    pub(super) idle_frame_millis_max: f32,
    /// The snapshot field whose change last asked for a frame, so the log
    /// names a churning reason once rather than every frame.
    pub(super) snapshot_changed_field: Option<String>,
    /// One liveness marker per module that registered a panel. The executable
    /// owns the strong handle, so a module reload - which the registry is told
    /// about - is what makes its panels read "module not loaded"; the module
    /// itself only queues requests.
    pub(super) panel_markers: std::collections::HashMap<String, Arc<ModuleLiveness>>,
    /// The newest request per panel id, kept so a panel can be registered
    /// again after an unload without the module running.
    pub(super) panel_requests: Vec<PanelRequest>,
}
/// What the last theme resolution read.
///
/// The executable is the side that resolves the layers: it reads the
/// `theme` preference, the user's file, `editor_settings.yaml` and the
/// project's layer through the asset store. Keeping what each read produced
/// is what lets a poll skip the merge when nothing moved.
#[derive(Default)]
pub(super) struct ThemeWatch {
    /// The preference's value, as last resolved.
    pub(super) selection: UserThemeSelection,
    /// The user file's metadata when it was last read.
    pub(super) user_stamp: FileStamp,
    /// The legacy name already reported, so one value logs once.
    pub(super) legacy_logged: Option<String>,
    /// `editor_settings.yaml`'s metadata when it was last read.
    pub(super) settings_stamp: FileStamp,
    /// The settings file's last read, or the message that refused it.
    pub(super) settings: Option<Result<pill_development_host::EditorSettings, String>>,
    /// The project layer's path, as last resolved.
    pub(super) project_path: Option<String>,
    /// The project layer's file metadata when it was last read.
    pub(super) project_stamp: FileStamp,
    /// The project layer's handle in the asset store.
    pub(super) project_handle: Option<pill_engine_core::Handle<ThemeOverrides>>,
    /// The handle's content version when it was last merged.
    pub(super) project_version: Option<u64>,
    /// Where each family's font comes from, as last resolved (body, display,
    /// monospace).
    pub(super) font_sources: [Option<FontSource>; 3],
    /// The resolved source files' metadata when they were last read.
    pub(super) font_stamps: [FileStamp; 3],
    /// The font bytes the sources gave, as last installed.
    pub(super) fonts: pill_editor_core::kit::style::ThemeFonts,
}
/// Where one font family's bytes come from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum FontSource {
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
pub(super) struct ScriptedDrag {
    /// The press position, in the main window's client pixels.
    pub(super) from: (f32, f32),
    /// The position the single move goes to.
    pub(super) to: (f32, f32),
    /// How far along the drag is.
    pub(super) stage: ScriptedDragStage,
}

/// The stage a [`ScriptedDrag`] is waiting to inject, one per frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ScriptedDragStage {
    /// The pointer has not gone down yet.
    Press,
    /// The button is down and the pointer has not moved yet.
    Move,
    /// The move landed and the button is still down.
    Release,
}

impl EditorWindow {
    /// Create the window, the editor's device, the host and the views.
    pub(super) fn open(
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
        let mut host = pill_development_host::setup_rendering(
            config,
            primary_window,
            primary_size.0,
            primary_size.1,
        )?;
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
            scene.attach(&mut host, &cameras[view]);
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
            preferences: StateFileWatch::new(layout_directory().join(PREFERENCES_FILE)),
            keybindings: StateFileWatch::new(layout_directory().join(KEYBINDINGS_FILE)),
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
}
