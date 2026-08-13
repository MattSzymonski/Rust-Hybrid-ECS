//! Windowed host — opens a `winit` + `wgpu` window and renders every
//! `(Position, Sprite)` entity via [`pill_engine::SpriteRenderer`] each frame.
//!
//! # Responsibilities
//!
//! - Create a GPU-accelerated window with `winit` and `wgpu`.
//! - Tick the shared [`host`] engine loop inside the window event loop.
//! - Draw sprites and update window title with FPS / entity count.

use std::sync::Arc;

use host::{run_one_frame, setup, GameModuleConfig, Host};
use pill_engine::SpriteRenderer;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

use super::error::StandaloneError;

// =============================================================================
// GraphicsState
// =============================================================================

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
    // -------------------------------------------------------------------------
    // Construction
    // -------------------------------------------------------------------------

    // Create a new `GraphicsState` with GPU resources for the given window.
    fn new(window: Arc<Window>) -> Result<Self, StandaloneError> {
        let size = window.inner_size();
        let instance = wgpu::Instance::default();
        let surface = instance
            .create_surface(window.clone())
            .map_err(|source| StandaloneError::SurfaceCreation { source })?;

        // Request a GPU adapter.
        // This is a blocking call, but it's only done once at startup, so it's acceptable.
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
        }))
        .map_err(|source| StandaloneError::AdapterUnavailable { source })?;

        // Request a device and queue from the adapter.
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: None,
            required_features: wgpu::Features::empty(),
            required_limits:
                wgpu::Limits::downlevel_webgl2_defaults().using_resolution(adapter.limits()),
            memory_hints: wgpu::MemoryHints::default(),
            ..Default::default()
        }))
        .map_err(|source| StandaloneError::DeviceCreation { source })?;

        // Configure the surface with a format and present mode.
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

        // Configure the surface with the selected format and present mode.
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

        // Create the sprite renderer.
        let sprite_renderer = SpriteRenderer::new(&device, surface_format);

        // Return the new GraphicsState.
        Ok(Self {
            window,
            surface,
            device,
            queue,
            surface_config,
            sprite_renderer,
        })
    }

    // -------------------------------------------------------------------------
    // Behavior
    // -------------------------------------------------------------------------

    // Resize the surface and reconfigure it for the new window size.
    fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.surface_config.width = width;
        self.surface_config.height = height;
        self.surface.configure(&self.device, &self.surface_config);
    }

    // Render the current frame by drawing all `(Position, Sprite)` entities.
    fn render(&mut self, host: &mut Host) {
        // Acquire the next frame from the surface.
        // If it fails, reconfigure the surface and return.
        let frame = match self.surface.get_current_texture() {
            Ok(frame) => frame,
            Err(_) => {
                self.surface.configure(&self.device, &self.surface_config);
                return;
            }
        };

        // Create a view of the frame's texture for rendering.
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        // Render all `(Position, Sprite)` entities using the sprite renderer.
        self.sprite_renderer.render(
            host.engine_mut().world_mut(),
            &self.device,
            &self.queue,
            &view,
            self.surface_config.width,
            self.surface_config.height,
        );

        // Present the frame to the window.
        frame.present();
    }
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

/// Application state for the windowed host. Created once in `run()`, then
/// passed to the `winit` event loop for the lifetime of the program.
struct App {
    module_config: GameModuleConfig,
    host: Option<Host>,
    graphics: Option<GraphicsState>,
    /// Failure recorded during `resumed`; surfaced after the loop exits.
    setup_error: Option<StandaloneError>,
}

impl ApplicationHandler for App {
    // Resume the application after a pause or suspension.
    // Initialize the host and graphics state if they haven't been created yet.
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.graphics.is_some() || self.setup_error.is_some() {
            return;
        }

        let window_attributes = Window::default_attributes()
            .with_title("ECS Standalone Host")
            .with_inner_size(winit::dpi::LogicalSize::new(800.0, 600.0));
        let window = match event_loop.create_window(window_attributes) {
            Ok(window) => Arc::new(window),
            Err(error) => {
                self.setup_error = Some(StandaloneError::WindowCreation { source: error });
                event_loop.exit();
                return;
            }
        };

        match setup(self.module_config.clone()) {
            Ok(host) => self.host = Some(host),
            Err(error) => {
                self.setup_error = Some(error.into());
                event_loop.exit();
                return;
            }
        }

        match GraphicsState::new(window) {
            Ok(graphics) => self.graphics = Some(graphics),
            Err(error) => {
                self.setup_error = Some(error);
                event_loop.exit();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        // Handle window events such as close, resize, and redraw.
        match event {
            // Only exit the event loop when the window is closed.
            WindowEvent::CloseRequested => {
                event_loop.exit();
            }
            // Resize the surface when the window is resized.
            WindowEvent::Resized(new_size) => {
                if let Some(graphics) = &mut self.graphics {
                    graphics.resize(new_size.width, new_size.height);
                }
            }
            // Render a new frame when the window requests a redraw.
            WindowEvent::RedrawRequested => {
                if let (Some(host), Some(graphics)) = (&mut self.host, &mut self.graphics) {
                    if let Some(report) = run_one_frame(host) {
                        // TODO: Remove this FPS logging
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
// Windowed mode entry point
// ---------------------------------------------------------------------------

/// Create a window, initialise the host, and run the render loop until the
/// window is closed.
pub fn run() -> Result<(), StandaloneError> {
    // Create a new event loop for the windowed application.
    let event_loop =
        EventLoop::new().map_err(|source| StandaloneError::EventLoopCreation { source })?;
    event_loop.set_control_flow(ControlFlow::Poll);

    // Create the application state and run the event loop.
    let mut app = App {
        module_config: GameModuleConfig::from_environment(),
        host: None,
        graphics: None,
        setup_error: None,
    };
    event_loop
        .run_app(&mut app)
        .map_err(|source| StandaloneError::EventLoopCreation { source })?;

    // Window-creation and GPU setup happen inside the event loop; surface
    // any deferred failure after the loop exits.
    if let Some(error) = app.setup_error {
        return Err(error);
    }
    Ok(())
}
