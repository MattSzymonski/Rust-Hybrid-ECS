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
use host::{setup_rendering, FrameReport, GameModuleConfig, RenderViewport, RenderingHost};

/// Maximum frequency at which live host statistics invalidate the Dioxus UI.
const STATS_UPDATE_INTERVAL: Duration = Duration::from_millis(100);

// These logical dimensions are shared by the CSS grid and the physical wgpu
// viewport calculation. Changing the layout requires updating both together.
const TOP_BAR_HEIGHT: f64 = 48.0;
const LEFT_PANEL_WIDTH: f64 = 220.0;
const RIGHT_PANEL_WIDTH: f64 = 260.0;
const BOTTOM_BAR_HEIGHT: f64 = 32.0;

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
#[derive(Debug, Clone, Copy, Default)]
struct Stats {
    fps: f64,
    entity_count: usize,
}

/// Build the editor UI and drive the host from Dioxus's native event loop.
fn app() -> Element {
    let editor = consume_context::<Arc<EditorContext>>();
    let mut stats = use_signal(Stats::default);

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

    use_wry_event_handler(move |event, _| {
        use dioxus::desktop::tao::event::WindowEvent;

        match event {
            TaoEvent::WindowEvent {
                event: WindowEvent::Resized(size),
                ..
            } => {
                editor.resize(size.width, size.height);
            }
            TaoEvent::RedrawRequested(_) => {
                if let Some(frame) = editor.render() {
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

    rsx! {
        div {
            position: "fixed",
            top: "0",
            right: "0",
            bottom: "0",
            left: "0",
            display: "grid",
            grid_template_columns: "220px minmax(0, 1fr) 260px",
            grid_template_rows: "48px minmax(0, 1fr) 32px",
            background_color: "transparent",

            // Opaque checker panels surround the transparent scene viewport.
            div {
                grid_column: "1 / -1",
                grid_row: "1",
                display: "flex",
                align_items: "center",
                padding: "0 16px",
                box_sizing: "border-box",
                background_color: "rgb(64, 64, 64)",
                background_image: "linear-gradient(45deg, rgba(255, 255, 255, 0.08) 25%, transparent 25%, transparent 75%, rgba(255, 255, 255, 0.08) 75%), linear-gradient(45deg, rgba(255, 255, 255, 0.08) 25%, transparent 25%, transparent 75%, rgba(255, 255, 255, 0.08) 75%)",
                background_size: "16px 16px",
                background_position: "0 0, 8px 8px",
                color: "white",
                font_family: "sans-serif",
                font_weight: "600",
                "ECS Editor"
            }
            div {
                grid_column: "1",
                grid_row: "2",
                background_color: "rgb(64, 64, 64)",
                background_image: "linear-gradient(45deg, rgba(255, 255, 255, 0.08) 25%, transparent 25%, transparent 75%, rgba(255, 255, 255, 0.08) 75%), linear-gradient(45deg, rgba(255, 255, 255, 0.08) 25%, transparent 25%, transparent 75%, rgba(255, 255, 255, 0.08) 75%)",
                background_size: "16px 16px",
                background_position: "0 0, 8px 8px",
            }
            div {
                grid_column: "2",
                grid_row: "2",
                position: "relative",
                overflow: "hidden",
                background_color: "transparent",
                box_shadow: "inset 0 0 0 1px rgba(255, 255, 255, 0.18)",
            }
            div {
                grid_column: "3",
                grid_row: "2",
                display: "flex",
                align_items: "flex-start",
                justify_content: "flex-end",
                background_color: "rgb(64, 64, 64)",
                background_image: "linear-gradient(45deg, rgba(255, 255, 255, 0.08) 25%, transparent 25%, transparent 75%, rgba(255, 255, 255, 0.08) 75%), linear-gradient(45deg, rgba(255, 255, 255, 0.08) 25%, transparent 25%, transparent 75%, rgba(255, 255, 255, 0.08) 75%)",
                background_size: "16px 16px",
                background_position: "0 0, 8px 8px",
                StatsWidget { stats }
            }
            div {
                grid_column: "1 / -1",
                grid_row: "3",
                background_color: "rgb(64, 64, 64)",
                background_image: "linear-gradient(45deg, rgba(255, 255, 255, 0.08) 25%, transparent 25%, transparent 75%, rgba(255, 255, 255, 0.08) 75%), linear-gradient(45deg, rgba(255, 255, 255, 0.08) 25%, transparent 25%, transparent 75%, rgba(255, 255, 255, 0.08) 75%)",
                background_size: "16px 16px",
                background_position: "0 0, 8px 8px",
            }
        }
    }
}

/// Display live engine statistics without invalidating the parent editor UI.
#[component]
fn StatsWidget(stats: Signal<Stats>) -> Element {
    let stats = stats.read();

    rsx! {
        div {
            margin: "12px",
            padding: "10px 14px",
            background_color: "rgba(20, 20, 20, 0.65)",
            color: "white",
            font_family: "monospace",
            font_size: "14px",
            border_radius: "6px",
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
        host.set_render_viewport(Some(editor_render_viewport(
            size.width,
            size.height,
            window.scale_factor(),
        )));

        Self {
            host: RefCell::new(host),
            window,
            last_stats_update: Cell::new(Instant::now()),
        }
    }

    /// Reconfigure the engine surface after Dioxus reports a physical resize.
    fn resize(&self, width: u32, height: u32) {
        let mut host = self.host.borrow_mut();
        host.resize(width, height);
        host.set_render_viewport(Some(editor_render_viewport(
            width,
            height,
            self.window.scale_factor(),
        )));
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

/// Convert the Dioxus grid's logical center cell into physical GPU pixels.
fn editor_render_viewport(width: u32, height: u32, scale_factor: f64) -> RenderViewport {
    let physical = |logical: f64| (logical * scale_factor).round() as u32;
    let left = physical(LEFT_PANEL_WIDTH);
    let right = physical(RIGHT_PANEL_WIDTH);
    let top = physical(TOP_BAR_HEIGHT);
    let bottom = physical(BOTTOM_BAR_HEIGHT);

    RenderViewport::new(
        left.min(width),
        top.min(height),
        width.saturating_sub(left.saturating_add(right)),
        height.saturating_sub(top.saturating_add(bottom)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The native viewport matches the center cell of the editor CSS grid.
    #[test]
    fn viewport_tracks_layout_and_scale_factor() {
        assert_eq!(
            editor_render_viewport(1280, 800, 1.0),
            RenderViewport::new(220, 48, 800, 720)
        );
        assert_eq!(
            editor_render_viewport(2560, 1600, 2.0),
            RenderViewport::new(440, 96, 1600, 1440)
        );
    }

    /// Very small windows yield an empty viewport instead of underflowing.
    #[test]
    fn viewport_saturates_for_small_windows() {
        assert_eq!(
            editor_render_viewport(320, 60, 1.0),
            RenderViewport::new(220, 48, 0, 0)
        );
    }
}
