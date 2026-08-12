//! Windowed host — opens a `winit` + `wgpu` window and renders every
//! `(Position, Sprite)` entity via [`ecs_hybrid::SpriteRenderer`] each frame.
//!
//! # Responsibilities
//!
//! - Create a GPU-accelerated window with `winit` and `wgpu`.
//! - Tick the shared [`host`] engine loop inside the window event loop.
//! - Draw sprites and update window title with FPS / entity count.

use std::sync::Arc;

use ecs_hybrid::SpriteRenderer;
use host::{run_one_frame, setup, GameModuleConfig, Host};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

// ---------------------------------------------------------------------------
// GraphicsState
// ---------------------------------------------------------------------------

/// GPU resources tied to the window's surface. Created lazily in
/// [`App::resumed`], since `winit` only hands out the window there.
struct GraphicsState {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface_config: wgpu::SurfaceConfiguration,
    sprite_renderer: SpriteRenderer,
}

impl GraphicsState {
    fn new(window: Arc<Window>) -> Self {
        let size = window.inner_size();
        let instance = wgpu::Instance::default();
        let surface = instance.create_surface(window.clone()).unwrap();

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
        }))
        .expect("failed to find a suitable GPU adapter");

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: None,
            required_features: wgpu::Features::empty(),
            required_limits:
                wgpu::Limits::downlevel_webgl2_defaults().using_resolution(adapter.limits()),
            memory_hints: wgpu::MemoryHints::default(),
            ..Default::default()
        }))
        .expect("failed to create GPU device");

        let capabilities = surface.get_capabilities(&adapter);
        let surface_format = capabilities.formats[0];
        let present_mode = if capabilities
            .present_modes
            .contains(&wgpu::PresentMode::Immediate)
        {
            wgpu::PresentMode::Immediate
        } else if capabilities
            .present_modes
            .contains(&wgpu::PresentMode::Mailbox)
        {
            wgpu::PresentMode::Mailbox
        } else {
            // Universally accepted; wgpu selects Immediate or Mailbox when
            // available and falls back to Fifo only when neither exists.
            wgpu::PresentMode::AutoNoVsync
        };
        println!("[render] Present mode: {present_mode:?}");

        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode,
            desired_maximum_frame_latency: 2,
            alpha_mode: capabilities.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(&device, &surface_config);

        let sprite_renderer = SpriteRenderer::new(&device, surface_format);

        Self {
            window,
            surface,
            device,
            queue,
            surface_config,
            sprite_renderer,
        }
    }

    fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.surface_config.width = width;
        self.surface_config.height = height;
        self.surface.configure(&self.device, &self.surface_config);
    }

    fn render(&mut self, host: &mut Host) {
        let frame = match self.surface.get_current_texture() {
            Ok(frame) => frame,
            Err(_) => {
                self.surface.configure(&self.device, &self.surface_config);
                return;
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        self.sprite_renderer.render(
            host.engine_mut().world_mut(),
            &self.device,
            &self.queue,
            &view,
            self.surface_config.width,
            self.surface_config.height,
        );

        frame.present();
    }
}

// ---------------------------------------------------------------------------
// App — winit ApplicationHandler
// ---------------------------------------------------------------------------

struct App {
    module_config: GameModuleConfig,
    host: Option<Host>,
    graphics: Option<GraphicsState>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.graphics.is_some() {
            return;
        }

        let window_attributes = Window::default_attributes()
            .with_title("ECS Standalone Host")
            .with_inner_size(winit::dpi::LogicalSize::new(800.0, 600.0));
        let window = Arc::new(
            event_loop
                .create_window(window_attributes)
                .expect("failed to create window"),
        );

        self.host = Some(setup(self.module_config.clone()).expect("host setup failed"));
        self.graphics = Some(GraphicsState::new(window));
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(new_size) => {
                if let Some(graphics) = &mut self.graphics {
                    graphics.resize(new_size.width, new_size.height);
                }
            }
            WindowEvent::RedrawRequested => {
                if let (Some(host), Some(graphics)) = (&mut self.host, &mut self.graphics) {
                    if let Some(report) = run_one_frame(host) {
                        println!(
                            "  {:>6.0} FPS | {:>5} entities",
                            report.fps, report.entity_count
                        );
                        graphics.window.set_title(&format!(
                            "ECS Standalone Host — {:.0} FPS | {} entities",
                            report.fps, report.entity_count
                        ));
                    }
                    graphics.render(host);
                    graphics.window.request_redraw();
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Create a window, initialise the host, and run the render loop until the
/// window is closed.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Poll);

    let mut app = App {
        module_config: GameModuleConfig::from_environment(),
        host: None,
        graphics: None,
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}
