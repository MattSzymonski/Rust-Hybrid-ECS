//! Dioxus editor with a live engine-rendered game viewport.
//!
//! # Design
//!
//! Dioxus owns the native window and its event loop. During window creation,
//! the editor passes an `Arc` clone of Dioxus's Tao window to
//! [`host::setup_rendering`]. The engine creates one GPU surface for that
//! window, while [`host::RenderingHost`] owns both engine and renderer state.
//! The editor only forwards resize and redraw events and draws its transparent
//! HTML diagnostics above the rendered game.

use std::cell::{Cell, RefCell};
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dioxus::desktop::tao::event::Event as TaoEvent;
use dioxus::desktop::tao::window::Window;
use dioxus::desktop::{use_wry_event_handler, window, Config};
use dioxus::prelude::*;
use futures_util::StreamExt;
use host::{setup_rendering, FrameReport, GameModuleConfig, RenderingHost};

/// Maximum frequency at which live host statistics invalidate the Dioxus UI.
const STATS_UPDATE_INTERVAL: Duration = Duration::from_millis(100);

/// Create the Dioxus window and attach a rendering host to that same window.
fn main() {
    let config = Config::new()
        .with_window(
            dioxus::desktop::tao::window::WindowBuilder::new()
                .with_title("ECS Editor")
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
            width: "100vw",
            height: "100vh",
            display: "flex",
            align_items: "flex-start",
            justify_content: "flex-end",
            StatsWidget { stats }
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
        let host = setup_rendering(
            GameModuleConfig::from_environment(),
            window,
            size.width,
            size.height,
        )
        .expect("editor rendering host setup failed");

        Self {
            host: RefCell::new(host),
            last_stats_update: Cell::new(Instant::now()),
        }
    }

    /// Reconfigure the engine surface after Dioxus reports a physical resize.
    fn resize(&self, width: u32, height: u32) {
        self.host.borrow_mut().resize(width, height);
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
