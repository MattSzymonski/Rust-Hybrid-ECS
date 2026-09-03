//! Dioxus editor with a live engine-rendered project viewport.
//!
//! # Design
//!
//! Dioxus owns the native window and its event loop. During window creation,
//! the editor passes an `Arc` clone of Dioxus's Tao window to
//! [`pill_host::setup_rendering`]. The engine creates one GPU surface for that
//! window, while [`pill_host::RenderingHost`] owns both engine and renderer state.
//! The editor forwards resize and redraw events, keeps its center viewport
//! transparent for the surface, and draws opaque HTML panels around it.

mod console_tab;
mod dock_view;
mod editor_state;
mod entities_tab;
mod error;
mod inspector;
mod layout;
mod popout;
mod systems_tab;

use std::cell::{Cell, RefCell};
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dioxus::desktop::tao::dpi::LogicalSize;
use dioxus::desktop::tao::event::Event as TaoEvent;
use dioxus::desktop::tao::window::{Window, WindowId};
use dioxus::desktop::{use_wry_event_handler, window, Config};
use dioxus::prelude::*;
use futures_util::StreamExt;
use pill_core::error::EngineMessage;
use pill_host::{
    engine_report, install_engine_report_handler, setup_rendering, EngineError, FrameReport,
    HostConfig, HostError, RenderViewport, RenderingHost, VirtualResolution,
};

use dock_view::DockView;
use editor_state::{EditorCommand, EditorSnapshot};
use error::EditorError;
use layout::{
    compute_layout, load_or_default, LayoutAction, LayoutMetrics, LayoutNode, PanelKind, Rect,
};
use pill_engine::Entity;
use popout::PopoutManager;

/// Maximum frequency at which live host statistics invalidate the Dioxus UI.
const STATS_UPDATE_INTERVAL: Duration = Duration::from_millis(100);

/// Cap for the console ring buffer of failed editor commands.
const COMMAND_ERROR_LIMIT: usize = 100;

/// Stable coordinate space used by the bouncing-ball project systems.
const PROJECT_VIRTUAL_RESOLUTION: VirtualResolution = VirtualResolution::new(800.0, 600.0);

/// Install the shared telemetry stack (terminal, optional file, optional
/// Tracy) before Dioxus takes over the event loop.
///
/// A file lane is added when `ECS_LOG_DIR` is set. The `profiling` feature
/// routes `profile::*` spans to Tracy through an independent filter.
fn init_telemetry() {
    use std::path::PathBuf;
    let file_directory = std::env::var_os("ECS_LOG_DIR").map(PathBuf::from);
    if let Err(error) = pill_host::init_telemetry(file_directory) {
        eprintln!("[editor] telemetry setup failed: {error}");
    }
}

/// Create the Dioxus window and attach a rendering host to that same window.
fn main() {
    install_engine_report_handler();
    init_telemetry();

    let config = Config::new()
        .with_disable_context_menu(true)
        .with_window(
            dioxus::desktop::tao::window::WindowBuilder::new()
                .with_title("ECS Editor")
                .with_inner_size(LogicalSize::new(1280.0, 800.0))
                .with_transparent(true),
        )
        .with_on_window(|window, dom| {
            // Dioxus retains event-loop ownership. The cloned Arc is passed to
            // the engine only so wgpu can keep the native surface alive.
            //
            // `EditorContext` is not `Send + Sync` - it owns a wgpu surface tied
            // to this window - so clippy suggests `Rc`. `Arc` is kept because
            // this handle travels through Dioxus's `provide_context` /
            // `consume_context` plumbing, and the atomic refcount is paid once
            // per window rather than on any hot path. Switching it would touch
            // GUI wiring that no automated test here can exercise.
            #[allow(clippy::arc_with_non_send_sync)]
            let context = match EditorContext::new(window) {
                Ok(context) => Arc::new(context),
                Err(error) => {
                    // The editor cannot render without its engine surface;
                    // report the typed failure once and stop the process.
                    // The local `mod error` (EditorError) occupies the module
                    // namespace, so `use pill_core::error;` would collide with
                    // it; call the flat-namespace macro by its full path.
                    pill_core::error!(
                        target: pill_core::telemetry::telemetry_target::ENGINE,
                        error = %error,
                        "editor rendering host setup failed"
                    );
                    eprintln!("{:?}", engine_report(error));
                    std::process::exit(1);
                }
            };
            dom.provide_root_context(context);
        })
        .with_as_child_window();

    dioxus::LaunchBuilder::desktop()
        .with_cfg(config)
        .launch(app);
}

/// Live statistics displayed by the transparent Dioxus overlay.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Stats {
    fps: f64,
    entity_count: usize,
}

/// Current WebView content size expressed in logical CSS pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
struct EditorSize {
    width: f64,
    height: f64,
}

/// Build the editor UI and drive the host from Dioxus's native event loop.
fn app() -> Element {
    let editor = consume_context::<Arc<EditorContext>>();
    let popouts = use_hook(|| Arc::new(PopoutManager::default()));
    let mut stats = use_signal(Stats::default);
    let layout_model = use_signal(load_or_default);
    let initial_size = editor.logical_size();
    let mut layout_size = use_signal(move || initial_size);

    // Defer each next-frame request through Dioxus's own scheduler. The yield
    // guarantees request_redraw runs in a later event-loop turn instead of
    // being coalesced into the RedrawRequested event currently in progress.
    // One completion produces one request, so this neither needs a timer nor
    // floods the runtime with a permanently self-waking task.
    let redraw_window = Arc::downgrade(&window().window);
    let redraw_scheduler = use_coroutine(move |mut requests: UnboundedReceiver<()>| {
        let redraw_window = redraw_window.clone();
        async move {
            while requests.next().await.is_some() {
                tokio::task::yield_now().await;
                let Some(window) = redraw_window.upgrade() else {
                    break;
                };
                window.request_redraw();
            }
        }
    });

    // Seed the first frame. Subsequent frames schedule themselves only after
    // their current engine update and presentation have completed.
    use_effect(move || redraw_scheduler.send(()));

    // Layout geometry is the shared source of truth for DOM positioning and
    // native GPU clipping. No DOM measurement round trip is required.
    let viewport_editor = Arc::clone(&editor);
    use_effect(move || {
        let size = *layout_size.read();
        let model = layout_model.read();
        let snapshot = compute_layout(
            &model,
            Rect::new(0.0, 0.0, size.width, size.height),
            LayoutMetrics::default(),
        );
        viewport_editor.set_scene_rect(snapshot.scene_rect);
    });

    let event_editor = Arc::clone(&editor);
    let event_popouts = Arc::clone(&popouts);
    use_wry_event_handler(move |event, _| {
        use dioxus::desktop::tao::event::WindowEvent;

        match event {
            TaoEvent::WindowEvent {
                event: WindowEvent::Resized(size),
                ..
            } => {
                event_editor.resize_main_window(size.width, size.height);
                layout_size.set(event_editor.logical_size());
            }
            TaoEvent::RedrawRequested(_) => {
                restore_detached_panels(layout_model, event_popouts.drain_redocks());
                if let Some(frame) = event_editor.render() {
                    if let Some(report) = frame.console_report {
                        println!(
                            "  {:>6.0} FPS | {:>5} entities",
                            report.fps, report.entity_count
                        );
                        let _ = std::io::stdout().flush();
                    }

                    // Only this signal write invalidates the overlay. The ECS
                    // and renderer continue running at their uncapped rate.
                    if let Some(report) = frame.ui_report {
                        stats.set(Stats {
                            fps: report.fps,
                            entity_count: report.entity_count,
                        });
                    }
                }
                redraw_scheduler.send(());
            }
            _ => {}
        }
    });

    let size = *layout_size.read();
    let model = layout_model.read();
    let snapshot = compute_layout(
        &model,
        Rect::new(0.0, 0.0, size.width, size.height),
        LayoutMetrics::default(),
    );
    drop(model);

    let undock_editor = Arc::clone(&editor);
    let undock_popouts = Arc::clone(&popouts);
    rsx! {
        DockView {
            model: layout_model,
            snapshot,
            stats,
            editor: Arc::clone(&editor),
            on_undock: move |panel| {
                popout::open_panel_window(
                    panel,
                    Arc::clone(&undock_editor),
                    Arc::clone(&undock_popouts),
                );
            }
        }
    }
}

/// Reinsert panels whose native pop-out windows have been closed.
fn restore_detached_panels(mut model: Signal<layout::LayoutModel>, panels: Vec<PanelKind>) {
    let mut changed = false;
    for panel in panels {
        let target_tabset = {
            let current = model.peek();
            if current
                .nodes
                .values()
                .any(|node| matches!(node, LayoutNode::Tab(tab) if tab.panel == panel))
            {
                continue;
            }
            current
                .active_tabset
                .filter(|id| current.tabset(*id).is_some())
                .or_else(|| {
                    current
                        .nodes
                        .iter()
                        .find_map(|(id, node)| matches!(node, LayoutNode::TabSet(_)).then_some(*id))
                })
        };
        let Some(target_tabset) = target_tabset else {
            continue;
        };
        match model.write().apply(LayoutAction::OpenTab {
            panel,
            target_tabset,
        }) {
            Ok(_) => changed = true,
            Err(error) => eprintln!("[editor] Could not redock {panel:?}: {error}"),
        }
    }
    if changed {
        layout::save(&model.peek());
    }
}

/// Display live engine statistics without invalidating the parent editor UI.
#[component]
pub(crate) fn StatsWidget(stats: Signal<Stats>) -> Element {
    let stats = stats.read();

    rsx! {
        div {
            class: "dock-statistics",
            div { "FPS: {stats.fps:.0}" }
            div { "Entities: {stats.entity_count}" }
        }
    }
}

/// Interior-mutable rendering host used from Dioxus's shared event callbacks.
pub(crate) struct EditorContext {
    host: RefCell<RenderingHost>,
    window: Arc<Window>,
    last_stats_update: Cell<Instant>,
    main_scene_viewport: Cell<RenderViewport>,
    detached_scene_window: Cell<Option<WindowId>>,
    /// Latest engine snapshot shared by every dock's VirtualDom.
    snapshot: RefCell<EditorSnapshot>,
    /// Structural and field commands queued by panels since the last frame.
    pending_commands: RefCell<Vec<EditorCommand>>,
    /// Ring buffer of the most recent command failures, shown in the Console.
    last_command_errors: RefCell<Vec<String>>,
    /// Entity the Inspector is showing; cleared when that entity dies.
    selection: Cell<Option<Entity>>,
    /// Throttle for snapshot captures (the engine keeps running uncapped).
    last_snapshot_refresh: Cell<Instant>,
}

impl PartialEq for EditorContext {
    /// Dioxus memoization compares component props between renders. The
    /// context is process-unique and every dock receives a clone of the same
    /// `Arc`, so equality reduces to a pointer check on the shared allocation.
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

/// Values produced while advancing one editor frame.
struct EditorFrame {
    console_report: Option<FrameReport>,
    ui_report: Option<FrameReport>,
}

impl EditorContext {
    /// Create one engine renderer surface from the Dioxus/Tao window handle.
    ///
    /// # Errors
    ///
    /// Returns the composed [`EditorError`] wrapping the host
    /// [`pill_host::EngineError`] when setup or GPU surface creation fails; the
    /// caller reports it once and exits.
    fn new(window: Arc<Window>) -> Result<Self, EditorError> {
        let size = window.inner_size();
        let mut host = setup_rendering(
            HostConfig::from_environment()
                .map_err(|source| EngineError::from(HostError::from(source)))?,
            Arc::clone(&window),
            size.width,
            size.height,
        )?;
        host.set_render_viewport(Some(RenderViewport::default()));
        host.set_render_virtual_resolution(Some(PROJECT_VIRTUAL_RESOLUTION));

        Ok(Self {
            host: RefCell::new(host),
            window,
            last_stats_update: Cell::new(Instant::now()),
            main_scene_viewport: Cell::new(RenderViewport::default()),
            detached_scene_window: Cell::new(None),
            snapshot: RefCell::new(EditorSnapshot::default()),
            pending_commands: RefCell::new(Vec::new()),
            last_command_errors: RefCell::new(Vec::new()),
            selection: Cell::new(None),
            last_snapshot_refresh: Cell::new(Instant::now()),
        })
    }

    /// Reconfigure the renderer only when it currently targets the main window.
    fn resize_main_window(&self, width: u32, height: u32) {
        if self.detached_scene_window.get().is_none() {
            self.host.borrow_mut().resize(width, height);
        }
    }

    /// Current WebView size in the logical coordinates used by CSS.
    fn logical_size(&self) -> EditorSize {
        let size = self.window.inner_size();
        let scale = self.window.scale_factor();
        EditorSize {
            width: size.width as f64 / scale,
            height: size.height as f64 / scale,
        }
    }

    /// Align native wgpu rendering to the selected Scene panel.
    fn set_scene_rect(&self, rect: Option<Rect>) {
        let viewport = rect
            .map(|rect| logical_rect_to_physical(rect, self.window.scale_factor()))
            .unwrap_or_default();
        self.main_scene_viewport.set(viewport);
        if self.detached_scene_window.get().is_none() {
            self.host.borrow_mut().set_render_viewport(Some(viewport));
        }
    }

    /// Move the live engine surface from the dock to a detached Scene window.
    pub(crate) fn attach_detached_scene(&self, window: Arc<Window>) -> Result<(), EditorError> {
        let size = window.inner_size();
        let window_id = window.id();
        let mut host = self.host.borrow_mut();
        host.retarget_render_window(window, size.width, size.height)
            .map_err(|source| EditorError::Retarget { source })?;
        host.set_render_virtual_resolution(Some(PROJECT_VIRTUAL_RESOLUTION));
        host.set_render_viewport(Some(RenderViewport::full(size.width, size.height)));
        self.detached_scene_window.set(Some(window_id));
        Ok(())
    }

    /// Resize the detached renderer without accepting events from stale windows.
    pub(crate) fn resize_detached_scene(&self, window_id: WindowId, width: u32, height: u32) {
        if self.detached_scene_window.get() == Some(window_id) {
            let mut host = self.host.borrow_mut();
            host.resize(width, height);
            host.set_render_viewport(Some(RenderViewport::full(width, height)));
        }
    }

    /// Return the live Scene renderer to the main editor window.
    pub(crate) fn reattach_main_scene(&self, detached_window: WindowId) -> Result<(), EditorError> {
        if self.detached_scene_window.get() != Some(detached_window) {
            return Ok(());
        }
        let size = self.window.inner_size();
        let mut host = self.host.borrow_mut();
        host.retarget_render_window(Arc::clone(&self.window), size.width, size.height)
            .map_err(|source| EditorError::Retarget { source })?;
        host.set_render_virtual_resolution(Some(PROJECT_VIRTUAL_RESOLUTION));
        host.set_render_viewport(Some(self.main_scene_viewport.get()));
        self.detached_scene_window.set(None);
        Ok(())
    }

    /// Snapshot engine statistics for a detached panel's isolated VirtualDom.
    pub(crate) fn current_stats(&self) -> Stats {
        let report = self.host.borrow().current_frame_report();
        Stats {
            fps: report.fps,
            entity_count: report.entity_count,
        }
    }

    /// Latest captured engine snapshot for the panels of any VirtualDom.
    pub(crate) fn snapshot(&self) -> EditorSnapshot {
        self.snapshot.borrow().clone()
    }

    /// Registered component types for the Inspector's add picker.
    pub(crate) fn registered_components(&self) -> Vec<editor_state::RegisteredComponent> {
        let host = self.host.borrow();
        editor_state::registered_components(host.engine().world())
    }

    /// Queue one editor command; it is applied at the next frame boundary.
    pub(crate) fn push_command(&self, command: EditorCommand) {
        self.pending_commands.borrow_mut().push(command);
    }

    /// Change Inspector selection; panels also use this to clear it.
    pub(crate) fn set_selection(&self, selection: Option<Entity>) {
        self.selection.set(selection);
    }

    /// Advance the ECS and present one frame on Dioxus's redraw event.
    ///
    /// Queued editor commands are applied first so this frame's systems see
    /// the world the user just arranged (structural commands are already
    /// visible; scalar writes take effect for `Changed<T>` the next frame,
    /// one accepted frame of latency).
    fn render(&self) -> Option<EditorFrame> {
        self.flush_pending_commands();

        let frame = {
            let mut host = self.host.borrow_mut();
            match host.run_one_frame() {
                Ok(console_report) => {
                    let now = Instant::now();
                    let ui_report = if now.duration_since(self.last_stats_update.get())
                        >= STATS_UPDATE_INTERVAL
                    {
                        self.last_stats_update.set(now);
                        Some(host.current_frame_report())
                    } else {
                        None
                    };

                    Some(EditorFrame {
                        console_report,
                        ui_report,
                    })
                }
                Err(error) => {
                    eprintln!(
                        "[editor] Fatal renderer error: {}",
                        EditorError::Frame { source: error }.to_plain_message()
                    );
                    None
                }
            }
        };

        // The host borrow has ended; capture a fresh snapshot without touching
        // the renderer.
        self.refresh_snapshot();
        frame
    }

    /// Apply the accumulated command batch right before systems run.
    fn flush_pending_commands(&self) {
        let commands = std::mem::take(&mut *self.pending_commands.borrow_mut());
        if commands.is_empty() {
            return;
        }
        let mut host = self.host.borrow_mut();
        let failures = EditorCommand::apply(host.engine_mut(), &commands);
        if !failures.is_empty() {
            let mut errors = self.last_command_errors.borrow_mut();
            for (_, message) in failures {
                if errors.len() >= COMMAND_ERROR_LIMIT {
                    errors.remove(0);
                }
                errors.push(message);
            }
        }
    }

    /// Re-capture the shared snapshot on the same cadence as the statistics.
    fn refresh_snapshot(&self) {
        let now = Instant::now();
        if now.duration_since(self.last_snapshot_refresh.get()) < STATS_UPDATE_INTERVAL {
            return;
        }
        self.last_snapshot_refresh.set(now);

        // Errors are drained here so they appear in exactly one snapshot and
        // never resurface on later refreshes.
        let errors = std::mem::take(&mut *self.last_command_errors.borrow_mut());

        let host = self.host.borrow();
        let module_names = host.optional_module_names();
        let revision = host.revision();
        let engine = host.engine();
        let mut fresh = EditorSnapshot::capture_list(engine, revision, &module_names, errors);
        let Some(selected) = self.selection.get() else {
            *self.snapshot.borrow_mut() = fresh;
            return;
        };
        if engine.world().is_entity_valid(selected) {
            fresh.detail = EditorSnapshot::capture_detail(engine, selected);
        } else {
            // The selected entity died; drop the selection rather than keep a
            // stale Inspector open.
            self.selection.set(None);
        }
        *self.snapshot.borrow_mut() = fresh;
    }
}

/// Convert a logical dock rectangle to stable physical edge coordinates.
fn logical_rect_to_physical(rect: Rect, scale_factor: f64) -> RenderViewport {
    let left = (rect.x * scale_factor).round().max(0.0) as u32;
    let top = (rect.y * scale_factor).round().max(0.0) as u32;
    let right = ((rect.x + rect.width) * scale_factor).round().max(0.0) as u32;
    let bottom = ((rect.y + rect.height) * scale_factor).round().max(0.0) as u32;
    RenderViewport::new(
        left,
        top,
        right.saturating_sub(left),
        bottom.saturating_sub(top),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Logical layout edges map consistently at common display scales.
    #[test]
    fn viewport_tracks_layout_and_scale_factor() {
        assert_eq!(
            logical_rect_to_physical(Rect::new(220.0, 48.0, 800.0, 720.0), 1.0),
            RenderViewport::new(220, 48, 800, 720),
        );
        assert_eq!(
            logical_rect_to_physical(Rect::new(220.0, 48.0, 800.0, 720.0), 2.0),
            RenderViewport::new(440, 96, 1600, 1440),
        );
    }

    /// Empty rectangles disable rendering without coordinate underflow.
    #[test]
    fn viewport_saturates_for_small_windows() {
        assert_eq!(
            logical_rect_to_physical(Rect::new(12.0, 20.0, 0.0, 0.0), 1.5),
            RenderViewport::new(18, 30, 0, 0),
        );
    }
}
