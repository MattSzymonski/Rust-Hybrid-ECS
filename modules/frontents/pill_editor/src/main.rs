//! Dioxus editor with a live engine-rendered project viewport.
//!
//! # Responsibilities
//!
//! - Own the native window and its event loop.
//! - Bridge the engine's renderer into that window.
//!
//! # Design
//!
//! Dioxus owns the native window and its event loop. During window creation,
//! the editor passes an `Arc` clone of Dioxus's Tao window to
//! [`pill_host::setup_rendering`]. The engine creates one GPU surface for that
//! window, while [`pill_host::RenderingHost`] - the development host with the
//! renderer attached - owns both engine and renderer state. The editor drives
//! it through [`pill_host::FrameDriver`], the same frame interface the
//! standalone frontend's loops use: it forwards resize and redraw events, keeps
//! its center viewport transparent for the surface, and draws opaque HTML
//! panels around it. Input over the scene reaches the WebView rather than the
//! window, so the Scene panel forwards its DOM events ([`scene_input`]); the
//! gamepads are polled around every frame.
//!
//! On Linux the surface is not the window itself but a native surface of the
//! engine's own placed at the Scene panel ([`scene_window`]), because there the
//! WebView and the surface would otherwise share one native window and
//! overwrite each other's pixels. On Wayland it sits below the transparent
//! WebView, as on Windows; on X11 it has to sit above it.

mod assets_tab;
mod console_tab;
mod dock_view;
mod editor_state;
mod entities_tab;
mod error;
mod inspector;
mod layout;
mod polling;
mod popout;
mod scene_input;
#[cfg(target_os = "linux")]
mod scene_window;
mod systems_tab;

use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::time::Duration;

use dioxus::desktop::tao::dpi::LogicalSize;
use dioxus::desktop::tao::event::Event as TaoEvent;
use dioxus::desktop::tao::window::{Window, WindowId};
use dioxus::desktop::{use_wry_event_handler, window, Config};
use dioxus::prelude::*;
use futures_util::StreamExt;
use pill_core::error::EngineMessage;
use pill_core::platform::Instant;
use pill_host::{
    engine_report, install_engine_report_handler, setup_rendering, FrameDriver, FrameReport,
    HostConfig, HostError, RenderViewport, RenderingError, RenderingHost,
};

use dock_view::DockView;
use editor_state::{EditorCommand, EditorSnapshot};
use error::EditorError;
use layout::{
    compute_layout, load_or_default, LayoutAction, LayoutMetrics, LayoutNode, PanelKind, Rect,
};
use pill_engine::{Entity, InputEvent};
use pill_input::gamepads::Gamepads;
use popout::PopoutManager;

/// Maximum frequency at which live host statistics invalidate the Dioxus UI.
const STATS_UPDATE_INTERVAL: Duration = Duration::from_millis(100);

/// Frame interval while the scene is not on screen. The renderer skips such
/// frames, so nothing else would pace the loop; this keeps the ECS ticking at
/// roughly a display's rate instead of spinning a core.
const HIDDEN_SCENE_FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// How soon to look again when the compositor has not yet shown the engine's
/// previous frame. Short against a frame, and a check costs next to nothing.
const SCENE_FRAME_POLL: Duration = Duration::from_millis(2);

/// Cap for the console ring buffer of failed editor commands.
const COMMAND_ERROR_LIMIT: usize = 100;

/// Install the shared telemetry stack (terminal, optional file, optional
/// Tracy) before Dioxus takes over the event loop.
///
/// A file lane is added when `ECS_LOG_DIR` is set. The `profiling` feature
/// routes `profile::*` spans to Tracy through an independent filter.
fn init_telemetry() {
    use std::path::PathBuf;
    let file_directory = std::env::var_os("ECS_LOG_DIR").map(PathBuf::from);
    if let Err(error) = pill_runtime::init_telemetry(file_directory) {
        // The one message that cannot go through the logger: it reports
        // that the logger itself did not install.
        eprintln!("[editor] telemetry setup failed: {error}");
    }
}

/// Create the Dioxus window and attach a rendering host to that same window.
fn main() {
    install_engine_report_handler();
    init_telemetry();

    let config = embedded_scene_config(Config::new())
        .with_disable_context_menu(true)
        .with_window(scene_window_builder(
            dioxus::desktop::tao::window::WindowBuilder::new()
                .with_title("ECS Editor")
                .with_inner_size(LogicalSize::new(1280.0, 800.0)),
        ))
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
                    pill_core::error!(target: pill_core::telemetry::telemetry_target::ENGINE, "{:?}", engine_report(error));
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

/// Whether a window that shows the scene has to be created transparent.
///
/// The engine presents underneath a transparent WebView everywhere except
/// Linux under X11, where its child window has to sit above the WebView
/// ([`scene_window`]) and a transparent GTK window would only give that child
/// an alpha channel to get wrong.
fn scene_needs_transparent_window() -> bool {
    #[cfg(target_os = "linux")]
    return scene_window::session_is_wayland();
    #[cfg(not(target_os = "linux"))]
    return true;
}

/// Configure a window that shows the scene through its WebView.
///
/// On Linux the GTK window's own background has to be transparent as well:
/// GTK 3 declares a window whose theme background is opaque as opaque to the
/// compositor (`wl_surface.set_opaque_region`), and the compositor then culls
/// the scene's subsurface underneath it - no pixels and no frame callbacks.
pub(crate) fn scene_window_builder(
    builder: dioxus::desktop::tao::window::WindowBuilder,
) -> dioxus::desktop::tao::window::WindowBuilder {
    if !scene_needs_transparent_window() {
        return builder;
    }
    let builder = builder.with_transparent(true);
    #[cfg(target_os = "linux")]
    let builder = builder.with_background_color((0, 0, 0, 0));
    builder
}

/// Adjust the configuration of a window that embeds the scene.
///
/// On Linux dioxus-desktop puts its default menu bar in the same GTK box as
/// the WebView, above it, so WebView (CSS) coordinates would no longer match
/// the window's and the scene would land one menu bar too high. Windows draws
/// its menu outside the client area and keeps the default.
pub(crate) fn embedded_scene_config(config: Config) -> Config {
    if cfg!(target_os = "linux") {
        config.with_menu(None::<dioxus::desktop::muda::Menu>)
    } else {
        config
    }
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
    // floods the runtime with a permanently self-waking task. A request can
    // carry a delay, for a frame that has to wait for the compositor.
    let redraw_window = Arc::downgrade(&window().window);
    let redraw_scheduler = use_coroutine(move |mut requests: UnboundedReceiver<Duration>| {
        let redraw_window = redraw_window.clone();
        async move {
            while let Some(delay) = requests.next().await {
                if delay.is_zero() {
                    tokio::task::yield_now().await;
                } else {
                    tokio::time::sleep(delay).await;
                }
                let Some(window) = redraw_window.upgrade() else {
                    break;
                };
                window.request_redraw();
            }
        }
    });

    // Seed the first frame. Subsequent frames schedule themselves only after
    // their current engine update and presentation have completed.
    use_effect(move || redraw_scheduler.send(Duration::ZERO));

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

    // Linux without X11 cannot put the engine and the WebView in one window.
    //
    // dioxus-desktop builds the WebView as a child window only on Windows,
    // macOS, iOS and Android (`build_as_child` in its `webview.rs`); everywhere
    // else the WebView is a GTK widget in the same window the engine's
    // swapchain presents to, and the two painters take turns replacing each
    // other's pixels. Under Wayland and X11 the engine gets a native surface of
    // its own and the Scene panel stays docked ([`scene_window`]). Where that
    // surface cannot be created, the Scene panel starts detached, in the window
    // where the engine is the only painter - which is what popping it out by
    // hand does. Set `PILL_EDITOR_DOCK_SCENE` to keep it docked anyway.
    //
    // The saved layout is why this runs on every start rather than once: the
    // Scene tab it removes is gone from the layout the next start loads, so the
    // window has to be opened whether or not there was a tab to detach. For
    // the same reason the docked path puts back a Scene tab an earlier detached
    // start saved away.
    #[cfg(target_os = "linux")]
    {
        let mut detach_layout = layout_model;
        let detach_editor = Arc::clone(&editor);
        let detach_popouts = Arc::clone(&popouts);
        let detached = use_hook(|| Cell::new(false));
        use_effect(move || {
            if detached.replace(true) {
                return;
            }
            if detach_editor.has_scene_window() {
                restore_detached_panels(detach_layout, vec![PanelKind::Scene]);
                return;
            }
            if std::env::var_os("PILL_EDITOR_DOCK_SCENE").is_some() {
                return;
            }
            let scene_tab = detach_layout.peek().nodes.iter().find_map(|(id, node)| {
                matches!(node, LayoutNode::Tab(tab) if tab.panel == PanelKind::Scene).then_some(*id)
            });
            if let Some(scene_tab) = scene_tab {
                // The write lock is released before the save, which reads.
                let detached = detach_layout
                    .write()
                    .apply(LayoutAction::DetachTab { tab: scene_tab });
                match detached {
                    Ok(_) => layout::save(&detach_layout.peek()),
                    Err(error) => {
                        eprintln!("[editor] Could not detach the Scene panel: {error}");
                        return;
                    }
                }
            }
            popout::open_panel_window(
                PanelKind::Scene,
                Arc::clone(&detach_editor),
                Arc::clone(&detach_popouts),
            );
        });
    }

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
                // GTK settles the WebView's size after the resize event that
                // announced it; catch up with it here.
                let size = event_editor.logical_size();
                if *layout_size.peek() != size {
                    layout_size.set(size);
                }
                if !event_editor.prepare_scene_frame() {
                    // The compositor has not shown the previous frame yet;
                    // look again shortly rather than block in the driver.
                    redraw_scheduler.send(SCENE_FRAME_POLL);
                    return;
                }
                let next_frame = event_editor.next_frame_delay();
                if let Some(frame) = event_editor.render() {
                    if let Some(report) = frame.console_report {
                        pill_core::info!(
                            target: pill_core::telemetry::telemetry_target::ENGINE,
                            "{:.0} FPS | {} entities",
                            report.fps,
                            report.entity_count
                        );
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
                redraw_scheduler.send(next_frame);
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
            current.resolved_active_tabset()
        };
        let Some(target_tabset) = target_tabset else {
            continue;
        };
        match model.write().apply(LayoutAction::OpenTab {
            panel,
            target_tabset,
        }) {
            Ok(_) => changed = true,
            Err(error) => {
                pill_core::warn!(target: pill_core::telemetry::telemetry_target::ENGINE, "Could not redock {panel:?}: {error}")
            }
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
    /// The main window's native scene window, when the platform provides one.
    #[cfg(target_os = "linux")]
    main_scene_child: Option<Arc<scene_window::SceneChildWindow>>,
    /// The detached Scene window's native scene window while it is open.
    #[cfg(target_os = "linux")]
    detached_scene_child: RefCell<Option<Arc<scene_window::SceneChildWindow>>>,
    /// Renderer size last given to the detached scene window.
    #[cfg(target_os = "linux")]
    detached_scene_size: Cell<(u32, u32)>,
    /// Latest engine snapshot shared by every dock's VirtualDom.
    snapshot: RefCell<EditorSnapshot>,
    /// Structural and field commands queued by panels since the last frame.
    pending_commands: RefCell<Vec<EditorCommand>>,
    /// Ring buffer of the most recent command failures, shown in the Console.
    last_command_errors: RefCell<Vec<String>>,
    /// Entity the Inspector is showing; cleared when that entity dies.
    selection: Cell<Option<Entity>>,
    /// Asset the Inspector is showing, by its path in `res`. Exclusive with
    /// [`Self::selection`]: selecting one clears the other, so the Inspector
    /// always shows what was clicked last.
    selected_asset: RefCell<Option<String>>,
    /// Throttle for snapshot captures (the engine keeps running uncapped).
    last_snapshot_refresh: Cell<Instant>,
    /// The gamepads, polled before every frame.
    gamepads: RefCell<Gamepads>,
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
    /// [`pill_host::RenderingError`] when setup or GPU surface creation fails;
    /// the caller reports it once and exits.
    fn new(window: Arc<Window>) -> Result<Self, EditorError> {
        let size = window.inner_size();
        let config = HostConfig::from_environment()
            .map_err(|source| RenderingError::from(HostError::from(source)))?;
        #[cfg(target_os = "linux")]
        let main_scene_child = scene_window::SceneChildWindow::new(&window).map(Arc::new);
        #[cfg(target_os = "linux")]
        let mut host = match &main_scene_child {
            Some(child) => {
                let mut host = setup_rendering(config, Arc::clone(child), 1, 1)?;
                host.resize(0, 0);
                host
            }
            None => setup_rendering(config, Arc::clone(&window), size.width, size.height)?,
        };
        #[cfg(not(target_os = "linux"))]
        let mut host = setup_rendering(config, Arc::clone(&window), size.width, size.height)?;
        FrameDriver::set_render_viewport(&mut host, Some(RenderViewport::default()));
        // The editor keeps every source asset in `res` paired with a `.meta`
        // file, so each one has a guid from the moment it is in the project.
        host.host_mut().set_ensure_asset_metadata(true);

        Ok(Self {
            host: RefCell::new(host),
            window,
            last_stats_update: Cell::new(Instant::now()),
            main_scene_viewport: Cell::new(RenderViewport::default()),
            detached_scene_window: Cell::new(None),
            #[cfg(target_os = "linux")]
            main_scene_child,
            #[cfg(target_os = "linux")]
            detached_scene_child: RefCell::new(None),
            #[cfg(target_os = "linux")]
            detached_scene_size: Cell::new((0, 0)),
            snapshot: RefCell::new(EditorSnapshot::default()),
            pending_commands: RefCell::new(Vec::new()),
            last_command_errors: RefCell::new(Vec::new()),
            selection: Cell::new(None),
            selected_asset: RefCell::new(None),
            last_snapshot_refresh: Cell::new(Instant::now()),
            gamepads: RefCell::new(Gamepads::new()),
        })
    }

    /// Whether the engine presents into a native window of its own rather
    /// than into the editor window it shares with the WebView.
    pub(crate) fn has_scene_window(&self) -> bool {
        #[cfg(target_os = "linux")]
        return self.main_scene_child.is_some();
        #[cfg(not(target_os = "linux"))]
        return false;
    }

    /// Reconfigure the renderer only when it currently targets the main window.
    ///
    /// A native scene window is sized by [`Self::set_scene_rect`] instead,
    /// which the layout calls after every main-window resize.
    fn resize_main_window(&self, width: u32, height: u32) {
        if self.detached_scene_window.get().is_none() && !self.has_scene_window() {
            FrameDriver::resize(&mut *self.host.borrow_mut(), width, height);
        }
    }

    /// Current WebView size in the logical coordinates used by CSS.
    ///
    /// On Linux this is measured on the WebView itself: tao's window size
    /// includes GTK's client-side decorations under Wayland.
    fn logical_size(&self) -> EditorSize {
        #[cfg(target_os = "linux")]
        if let Some((width, height)) = scene_window::webview_logical_size(&self.window) {
            return EditorSize { width, height };
        }
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
        #[cfg(target_os = "linux")]
        if let Some(child) = &self.main_scene_child {
            // The child covers the panel, so the renderer fills all of it.
            // While detached the panel has no tab and the child stays hidden.
            let (width, height) = child.place(viewport);
            if self.detached_scene_window.get().is_none() {
                let mut host = self.host.borrow_mut();
                host.resize(width, height);
                host.set_render_viewport(Some(RenderViewport::full(width, height)));
            }
            return;
        }
        if self.detached_scene_window.get().is_none() {
            FrameDriver::set_render_viewport(&mut *self.host.borrow_mut(), Some(viewport));
        }
    }

    /// Move the live engine surface from the dock to a detached Scene window.
    pub(crate) fn attach_detached_scene(&self, window: Arc<Window>) -> Result<(), EditorError> {
        let size = window.inner_size();
        let window_id = window.id();
        let mut host = self.host.borrow_mut();
        #[cfg(target_os = "linux")]
        if self.has_scene_window() {
            // The pop-out has a WebView too; give the engine a child window
            // of it for the same reason the dock has one.
            let child = scene_window::SceneChildWindow::new(&window)
                .map(Arc::new)
                .ok_or(EditorError::SceneWindow)?;
            let (width, height) = child.webview_size();
            let (width, height) = child.place(RenderViewport::full(width, height));
            host.retarget_render_window(Arc::clone(&child), width.max(1), height.max(1))
                .map_err(|source| EditorError::Retarget { source })?;
            host.resize(width, height);
            host.set_render_viewport(Some(RenderViewport::full(width, height)));
            self.detached_scene_size.set((width, height));
            *self.detached_scene_child.borrow_mut() = Some(child);
            self.detached_scene_window.set(Some(window_id));
            return Ok(());
        }
        host.retarget_render_window(window, size.width, size.height)
            .map_err(|source| EditorError::Retarget { source })?;
        FrameDriver::set_render_viewport(
            &mut *host,
            Some(RenderViewport::full(size.width, size.height)),
        );
        self.detached_scene_window.set(Some(window_id));
        Ok(())
    }

    /// Resize the detached renderer without accepting events from stale windows.
    pub(crate) fn resize_detached_scene(&self, window_id: WindowId, width: u32, height: u32) {
        if self.detached_scene_window.get() == Some(window_id) {
            // The detached scene window follows its WebView every frame
            // instead ([`Self::refresh_scene_window`]).
            #[cfg(target_os = "linux")]
            if self.detached_scene_child.borrow().is_some() {
                return;
            }
            let mut host = self.host.borrow_mut();
            FrameDriver::resize(&mut *host, width, height);
            FrameDriver::set_render_viewport(&mut *host, Some(RenderViewport::full(width, height)));
        }
    }

    /// Return the live Scene renderer to the main editor window.
    pub(crate) fn reattach_main_scene(&self, detached_window: WindowId) -> Result<(), EditorError> {
        if self.detached_scene_window.get() != Some(detached_window) {
            return Ok(());
        }
        let mut host = self.host.borrow_mut();
        #[cfg(target_os = "linux")]
        if let Some(child) = &self.main_scene_child {
            let (width, height) = child.place(self.main_scene_viewport.get());
            // The swapchain is built at least 1x1 and parked by the resize
            // when the panel is not on screen yet; the redock re-places it.
            host.retarget_render_window(Arc::clone(child), width.max(1), height.max(1))
                .map_err(|source| EditorError::Retarget { source })?;
            host.resize(width, height);
            host.set_render_viewport(Some(RenderViewport::full(width, height)));
            // The old renderer, and with it the last use of this window, has
            // been dropped by the retarget.
            self.detached_scene_child.borrow_mut().take();
            self.detached_scene_window.set(None);
            return Ok(());
        }
        let size = self.window.inner_size();
        host.retarget_render_window(Arc::clone(&self.window), size.width, size.height)
            .map_err(|source| EditorError::Retarget { source })?;
        FrameDriver::set_render_viewport(&mut *host, Some(self.main_scene_viewport.get()));
        self.detached_scene_window.set(None);
        Ok(())
    }

    /// Snapshot engine statistics for a detached panel's isolated VirtualDom.
    pub(crate) fn current_stats(&self) -> Stats {
        let report = self.host.borrow().host().current_frame_report();
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
        editor_state::registered_components(host.host().engine().world())
    }

    /// The project's `res` tree, for the Assets panel.
    pub(crate) fn asset_entries(&self) -> Vec<pill_host::AssetEntry> {
        self.host.borrow().host().asset_entries()
    }

    /// An asset's import settings (or standalone document) as JSON.
    pub(crate) fn asset_settings(&self, path: &str) -> Result<serde_json::Value, String> {
        self.host.borrow().host().asset_settings(path)
    }

    /// Save an asset's settings; the running scene picks them up at the next
    /// frame through the host's asset watcher.
    pub(crate) fn save_asset_settings(
        &self,
        path: &str,
        settings: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        self.host
            .borrow_mut()
            .host_mut()
            .save_asset_settings(path, settings)
    }

    /// The standalone asset types the Create dialog offers.
    pub(crate) fn standalone_asset_types(&self) -> Vec<pill_host::StandaloneType> {
        self.host.borrow().host().standalone_asset_types()
    }

    /// Create and load a new standalone asset; returns its path in `res`.
    pub(crate) fn create_standalone_asset(
        &self,
        type_name: &str,
        folder: &str,
        name: &str,
    ) -> Result<String, String> {
        self.host
            .borrow_mut()
            .host_mut()
            .create_standalone_asset(type_name, folder, name)
    }

    /// Move an asset with its `.meta` inside `res`.
    pub(crate) fn move_asset(&self, from: &str, to: &str) -> Result<(), String> {
        self.host.borrow().host().move_asset(from, to)
    }

    /// Queue one editor command; it is applied at the next frame boundary.
    pub(crate) fn push_command(&self, command: EditorCommand) {
        self.pending_commands.borrow_mut().push(command);
    }

    /// Queue an input event from the Scene panel for the next frame.
    pub(crate) fn push_input(&self, event: InputEvent) {
        FrameDriver::push_input(&mut *self.host.borrow_mut(), event);
    }

    /// Change Inspector selection; panels also use this to clear it.
    ///
    /// Selecting an entity clears the selected asset.
    pub(crate) fn set_selection(&self, selection: Option<Entity>) {
        if selection.is_some() {
            *self.selected_asset.borrow_mut() = None;
        }
        self.selection.set(selection);
    }

    /// Select the asset at `path` (relative to `res`) for the Inspector, or
    /// clear the asset selection. Selecting an asset clears the selected
    /// entity.
    pub(crate) fn select_asset(&self, path: Option<String>) {
        if path.is_some() {
            self.selection.set(None);
        }
        *self.selected_asset.borrow_mut() = path;
    }

    /// The asset the Inspector is showing, if any.
    pub(crate) fn selected_asset(&self) -> Option<String> {
        self.selected_asset.borrow().clone()
    }

    /// Advance the ECS and present one frame on Dioxus's redraw event.
    ///
    /// Queued editor commands are applied first so this frame's systems see
    /// the world the user just arranged (structural commands are already
    /// visible; scalar writes take effect for `Changed<T>` the next frame,
    /// one accepted frame of latency). The gamepads are polled just before the
    /// frame and play the rumble it requested just after.
    /// Call only after [`Self::prepare_scene_frame`] allowed the frame.
    fn render(&self) -> Option<EditorFrame> {
        self.flush_pending_commands();

        let frame = {
            let mut host = self.host.borrow_mut();
            let mut gamepads = self.gamepads.borrow_mut();
            gamepads.poll(|input| FrameDriver::push_input(&mut *host, input));
            let result = host.run_frame();
            #[cfg(target_os = "linux")]
            if let Some(child) = self.current_scene_child() {
                child.end_frame(host.presented_last_frame());
            }
            match result {
                Ok(console_report) => {
                    gamepads.play_rumble(FrameDriver::take_rumble_requests(&mut *host));
                    let now = Instant::now();
                    let ui_report = if now.duration_since(self.last_stats_update.get())
                        >= STATS_UPDATE_INTERVAL
                    {
                        self.last_stats_update.set(now);
                        Some(host.host().current_frame_report())
                    } else {
                        None
                    };

                    Some(EditorFrame {
                        console_report,
                        ui_report,
                    })
                }
                Err(error) => {
                    pill_core::error!(
                        target: pill_core::telemetry::telemetry_target::ENGINE,
                        "Fatal renderer error: {}",
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

    /// Per-frame upkeep of the engine's native scene window, and whether the
    /// engine may run its next frame now.
    ///
    /// A detached Scene window fills its WebView, whose size GTK settles only
    /// after the resize event that announced it, so it is measured here. The
    /// answer is `false` while the compositor has not shown the previous frame
    /// ([`scene_window::SceneChildWindow::begin_frame`]); the caller retries
    /// shortly instead of blocking the event loop inside the driver.
    fn prepare_scene_frame(&self) -> bool {
        #[cfg(target_os = "linux")]
        if let Some(child) = self.current_scene_child() {
            if self.detached_scene_window.get().is_some() {
                let (width, height) = child.webview_size();
                let (width, height) = child.place(RenderViewport::full(width, height));
                if self.detached_scene_size.replace((width, height)) != (width, height) {
                    let mut host = self.host.borrow_mut();
                    host.resize(width, height);
                    host.set_render_viewport(Some(RenderViewport::full(width, height)));
                }
            }
            child.refresh();
            return child.begin_frame();
        }
        true
    }

    /// How long to wait before the next frame: nothing while the engine
    /// presents (the compositor paces it), a display interval while the scene
    /// is off screen and every frame would be skipped.
    fn next_frame_delay(&self) -> Duration {
        #[cfg(target_os = "linux")]
        if self
            .current_scene_child()
            .is_some_and(|child| !child.is_shown())
        {
            return HIDDEN_SCENE_FRAME_INTERVAL;
        }
        Duration::ZERO
    }

    /// The native scene window the renderer currently presents into.
    #[cfg(target_os = "linux")]
    fn current_scene_child(&self) -> Option<Arc<scene_window::SceneChildWindow>> {
        if self.detached_scene_window.get().is_some() {
            self.detached_scene_child.borrow().clone()
        } else {
            self.main_scene_child.clone()
        }
    }

    /// Apply the accumulated command batch right before systems run.
    fn flush_pending_commands(&self) {
        let commands = std::mem::take(&mut *self.pending_commands.borrow_mut());
        if commands.is_empty() {
            return;
        }
        let mut host = self.host.borrow_mut();
        let failures = EditorCommand::apply(host.host_mut().engine_mut(), &commands);
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
        let module_names = host.host().extension_names();
        let revision = host.host().revision();
        let engine = host.host().engine();
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
