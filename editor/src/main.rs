//! Dioxus editor with a live engine-rendered game viewport.
//!
//! # Design
//!
//! Dioxus owns the native window and its event loop. During window creation,
//! the editor passes an `Arc` clone of Dioxus's Tao window to
//! [`host::setup_rendering`]. The engine creates one GPU surface for that
//! window, while [`host::RenderingHost`] owns both engine and renderer state.
//! The editor forwards resize and redraw events, keeps its center viewport
//! transparent for the surface, and draws opaque HTML panels around it.

mod dock_view;
mod layout;

use std::cell::{Cell, RefCell};
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dioxus::desktop::tao::dpi::LogicalSize;
use dioxus::desktop::tao::event::Event as TaoEvent;
use dioxus::desktop::tao::window::Window;
use dioxus::desktop::{use_wry_event_handler, window, Config};
use dioxus::prelude::*;
use futures_util::StreamExt;
use host::{
    setup_rendering, FrameReport, GameModuleConfig, RenderViewport, RenderingHost,
    VirtualResolution,
};

use dock_view::DockView;
use layout::{compute_layout, load_or_default, LayoutMetrics, Rect};

/// Maximum frequency at which live host statistics invalidate the Dioxus UI.
const STATS_UPDATE_INTERVAL: Duration = Duration::from_millis(100);

/// Stable coordinate space used by the bouncing-ball game systems.
const GAME_VIRTUAL_RESOLUTION: VirtualResolution = VirtualResolution::new(800.0, 600.0);

/// Create the Dioxus window and attach a rendering host to that same window.
fn main() {
    let config = Config::new()
        .with_window(
            dioxus::desktop::tao::window::WindowBuilder::new()
                .with_title("ECS Editor")
                .with_inner_size(LogicalSize::new(1280.0, 800.0))
                .with_transparent(true),
        )
        .with_on_window(|window, dom| {
            // Dioxus retains event-loop ownership. The cloned Arc is passed to
            // the engine only so wgpu can keep the native surface alive.
            let context = Arc::new(EditorContext::new(window));
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
    use_wry_event_handler(move |event, _| {
        use dioxus::desktop::tao::event::WindowEvent;

        match event {
            TaoEvent::WindowEvent {
                event: WindowEvent::Resized(size),
                ..
            } => {
                event_editor.resize(size.width, size.height);
                layout_size.set(event_editor.logical_size());
            }
            TaoEvent::RedrawRequested(_) => {
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

    rsx! {
        DockView { model: layout_model, snapshot, stats }
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
struct EditorContext {
    host: RefCell<RenderingHost>,
    window: Arc<Window>,
    last_stats_update: Cell<Instant>,
}

/// Values produced while advancing one editor frame.
struct EditorFrame {
    console_report: Option<FrameReport>,
    ui_report: Option<FrameReport>,
}

impl EditorContext {
    /// Create one engine renderer surface from the Dioxus/Tao window handle.
    fn new(window: Arc<Window>) -> Self {
        let size = window.inner_size();
        let mut host = setup_rendering(
            GameModuleConfig::from_environment(),
            Arc::clone(&window),
            size.width,
            size.height,
        )
        .expect("editor rendering host setup failed");
        host.set_render_viewport(Some(RenderViewport::default()));
        host.set_render_virtual_resolution(Some(GAME_VIRTUAL_RESOLUTION));

        Self {
            host: RefCell::new(host),
            window,
            last_stats_update: Cell::new(Instant::now()),
        }
    }

    /// Reconfigure the engine surface after Dioxus reports a physical resize.
    fn resize(&self, width: u32, height: u32) {
        self.host.borrow_mut().resize(width, height);
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
        self.host.borrow_mut().set_render_viewport(Some(viewport));
    }

    /// Advance the ECS and present one frame on Dioxus's redraw event.
    fn render(&self) -> Option<EditorFrame> {
        let mut host = self.host.borrow_mut();
        match host.run_one_frame() {
            Ok(console_report) => {
                let now = Instant::now();
                let ui_report =
                    if now.duration_since(self.last_stats_update.get()) >= STATS_UPDATE_INTERVAL {
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
                eprintln!("[editor] Fatal renderer error: {error}");
                None
            }
        }
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
