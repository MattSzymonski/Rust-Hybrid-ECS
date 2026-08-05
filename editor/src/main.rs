//! Editor — a Dioxus desktop app with a live game viewport and details panel.
//!
//! # Design
//!
//! Dioxus owns the single native window (`with_as_child_window()` +
//! `with_transparent(true)`), following the pattern in dioxus's own
//! `examples/08-apis/wgpu_child_window.rs`. A `wgpu` surface is created
//! against that same window and draws the running game's sprites; the
//! transparent HTML overlay on top shows FPS and entity count.
//!
//! The engine and game DLL hot-reload loop live in the shared [`host`]
//! crate. Each redraw runs one engine frame ([`host::run_one_frame`]) then
//! renders it, so the viewport, engine simulation, and UI panel all advance
//! together on Dioxus's own event loop - there is no separate render thread.

use std::cell::RefCell;
use std::sync::Arc;

use dioxus::desktop::tao::event::Event as WryEvent;
use dioxus::desktop::tao::window::Window;
use dioxus::desktop::{use_wry_event_handler, window, Config};
use dioxus::prelude::*;
use ecs_hybrid::SpriteRenderer;
use host::{run_one_frame, setup, GameModuleConfig, Host};

fn main() {
    let config = Config::new()
        .with_window(dioxus::desktop::tao::window::WindowBuilder::new()
            .with_title("ECS Editor")
            .with_transparent(true))
        .with_on_window(|window, dom| {
            let context = Arc::new(pollster::block_on(async {
                let context = EditorContextAsyncBuilder {
                    desktop: window,
                    resources_builder: |ctx| Box::pin(EditorResources::new(ctx.clone())),
                }
                .build()
                .await;

                context
            }));

            dom.provide_root_context(context);
        })
        .with_as_child_window();

    dioxus::LaunchBuilder::desktop()
        .with_cfg(config)
        .launch(app);
}

/// Live stats read by the details panel. Updated once per rendered frame.
#[derive(Debug, Clone, Copy, Default)]
struct Stats {
    fps: f64,
    entity_count: usize,
}

fn app() -> Element {
    let editor_context = consume_context::<Arc<EditorContext>>();
    let mut stats = use_signal(Stats::default);

    // Kick off the render loop with an initial redraw request; each
    // RedrawRequested we process one engine frame, render it, and request
    // the next redraw, so the loop keeps going on its own.
    use_effect(move || {
        window().window.request_redraw();
    });

    use_wry_event_handler(move |event, _| {
        use dioxus::desktop::tao::event::WindowEvent;

        match event {
            WryEvent::WindowEvent {
                event: WindowEvent::Resized(new_size),
                ..
            } => {
                editor_context.with_resources(|resources| {
                    resources.resize(new_size.width, new_size.height);
                });
                window().window.request_redraw();
            }
            WryEvent::RedrawRequested(_) => {
                let report = editor_context.with_resources(|resources| resources.render());
                if let Some(report) = report {
                    stats.set(Stats {
                        fps: report.fps,
                        entity_count: report.entity_count,
                    });
                }
                window().window.request_redraw();
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

/// This borrows from the `window` which is contained within an `Arc`, so it
/// needs to be a self-referencing struct to be able to borrow the window for
/// the wgpu::Surface.
#[ouroboros::self_referencing]
struct EditorContext {
    desktop: Arc<Window>,
    #[borrows(desktop)]
    #[not_covariant]
    resources: EditorResources<'this>,
}

/// GPU + engine state, borrowing the Dioxus-owned window for the wgpu surface.
///
/// Mutable fields are wrapped in `RefCell` because `resize()`/`render()` are
/// called through a shared `&self` borrow (ouroboros only exposes
/// `with_resources` with a shared reference to the borrowing struct).
struct EditorResources<'a> {
    surface: wgpu::Surface<'a>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface_config: RefCell<wgpu::SurfaceConfiguration>,
    sprite_renderer: RefCell<SpriteRenderer>,
    host: RefCell<Host>,
}

impl<'a> EditorResources<'a> {
    async fn new(window: Arc<Window>) -> Self {
        let size = window.inner_size();

        let instance = wgpu::Instance::default();
        let surface: wgpu::Surface<'a> = instance.create_surface(window).unwrap();

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::default(),
                force_fallback_adapter: false,
                compatible_surface: Some(&surface),
            })
            .await
            .expect("failed to find a suitable GPU adapter");

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: None,
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_webgl2_defaults()
                    .using_resolution(adapter.limits()),
                memory_hints: wgpu::MemoryHints::default(),
                ..Default::default()
            })
            .await
            .expect("failed to create GPU device");

        let capabilities = surface.get_capabilities(&adapter);
        let surface_format = capabilities.formats[0];

        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            // Prefer a blending alpha mode so the wgpu-rendered viewport
            // shows through wherever the Dioxus overlay is transparent
            // HTML; not every backend (notably DX12 on Windows) supports
            // one, so fall back to whatever the surface actually reports.
            alpha_mode: capabilities
                .alpha_modes
                .iter()
                .copied()
                .find(|mode| {
                    matches!(
                        mode,
                        wgpu::CompositeAlphaMode::PostMultiplied
                            | wgpu::CompositeAlphaMode::PreMultiplied
                    )
                })
                .unwrap_or(capabilities.alpha_modes[0]),
            view_formats: vec![],
        };
        surface.configure(&device, &surface_config);

        let sprite_renderer = SpriteRenderer::new(&device, surface_format);

        let host = setup(GameModuleConfig::from_environment()).expect("host setup failed");

        Self {
            surface,
            device,
            queue,
            surface_config: RefCell::new(surface_config),
            sprite_renderer: RefCell::new(sprite_renderer),
            host: RefCell::new(host),
        }
    }

    fn resize(&self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        let mut surface_config = self.surface_config.borrow_mut();
        surface_config.width = width;
        surface_config.height = height;
        self.surface.configure(&self.device, &surface_config);
    }

    /// Run one engine frame and draw it. Returns a fresh [`host::FrameReport`]
    /// when one was due this frame (roughly every 2 seconds).
    fn render(&self) -> Option<host::FrameReport> {
        let mut host = self.host.borrow_mut();
        let report = run_one_frame(&mut host);

        let surface_config = self.surface_config.borrow();
        let frame = match self.surface.get_current_texture() {
            Ok(frame) => frame,
            Err(_) => {
                self.surface.configure(&self.device, &surface_config);
                return report;
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        self.sprite_renderer.borrow_mut().render(
            host.engine_mut().world_mut(),
            &self.device,
            &self.queue,
            &view,
            surface_config.width,
            surface_config.height,
        );

        frame.present();
        report
    }
}
