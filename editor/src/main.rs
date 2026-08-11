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

use std::cell::RefCell;
use std::sync::Arc;

use dioxus::desktop::tao::event::Event as TaoEvent;
use dioxus::desktop::tao::window::Window;
use dioxus::desktop::{use_wry_event_handler, Config};
use dioxus::prelude::*;
use host::{setup_rendering, FrameReport, GameModuleConfig, RenderingHost};

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
                if let Some(report) = editor.render() {
                    stats.set(Stats {
                        fps: report.fps,
                        entity_count: report.entity_count,
                    });
                }
            }
            _ => {}
        }
    });

    let stats = stats.read();
    rsx! {
        div {
            width: "100vw",
            height: "100vh",
            display: "flex",
            align_items: "flex-start",
            justify_content: "flex-end",
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
}

/// Interior-mutable rendering host used from Dioxus's shared event callbacks.
struct EditorContext {
    host: RefCell<RenderingHost>,
}

impl EditorContext {
    /// Create one engine renderer surface from the Dioxus/Tao window handle.
    fn new(window: Arc<Window>) -> Self {
        let size = window.inner_size();
        let host = setup_rendering(
            GameModuleConfig::from_environment(),
            Arc::clone(&window),
            size.width,
            size.height,
        )
        .expect("editor rendering host setup failed");

        // Dioxus resets Tao to ControlFlow::Wait after each event. Let the
        // host asynchronously wake that loop after every presented frame so
        // redraw requests cannot be coalesced inside RedrawRequested.
        let weak_window = Arc::downgrade(&window);
        let mut host = host;
        host.start_continuous_rendering(move || {
            if let Some(window) = weak_window.upgrade() {
                window.request_redraw();
            }
        });

        Self {
            host: RefCell::new(host),
        }
    }

    /// Reconfigure the engine surface after Dioxus reports a physical resize.
    fn resize(&self, width: u32, height: u32) {
        self.host.borrow_mut().resize(width, height);
    }

    /// Advance the ECS and present one frame on Dioxus's redraw event.
    fn render(&self) -> Option<FrameReport> {
        match self.host.borrow_mut().run_one_frame() {
            Ok(report) => report,
            Err(error) => {
                eprintln!("[editor] Fatal renderer error: {error}");
                None
            }
        }
    }
}
