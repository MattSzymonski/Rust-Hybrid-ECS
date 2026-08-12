//! Complete standalone application runner owned by the host crate.
//!
//! The non-rendering build drives frames in a tight headless loop. The
//! rendering build owns `winit`, creates the native window, asks host setup to
//! attach the engine renderer, and forwards resize/redraw events.
//!
//! # Responsibilities
//!
//! - Run the configured game in a headless loop when rendering is disabled.
//! - Run the configured game in a `winit` window when rendering is enabled.

// Standard library
#[cfg(feature = "rendering")]
use std::sync::Arc;

// External crates
#[cfg(feature = "rendering")]
use winit::application::ApplicationHandler;
#[cfg(feature = "rendering")]
use winit::event::WindowEvent;
#[cfg(feature = "rendering")]
use winit::event_loop::ActiveEventLoop;
#[cfg(feature = "rendering")]
use winit::window::{Window, WindowId};

// Current crate
use crate::GameModuleConfig;

// =============================================================================
// Types + Impls
// =============================================================================

/// State retained by `winit` for the lifetime of the standalone application.
#[cfg(feature = "rendering")]
struct WindowedApplication {
    module_config: GameModuleConfig,
    window: Option<Arc<Window>>,
    host: Option<crate::RenderingHost>,
}

#[cfg(feature = "rendering")]
impl ApplicationHandler for WindowedApplication {
    /// Create the native window and complete host/renderer setup on resume.
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        // `winit` only permits creating a window while the event loop is
        // active, so this is the first point where rendering setup can finish.
        let attributes = Window::default_attributes()
            .with_title("ECS Standalone Host")
            .with_inner_size(winit::dpi::LogicalSize::new(800.0, 600.0));
        let window = Arc::new(
            event_loop
                .create_window(attributes)
                .expect("failed to create standalone host window"),
        );
        let size = window.inner_size();

        // Renderer construction is part of host setup; this runner supplies
        // only the platform window that wgpu needs for its surface.
        let host = crate::setup_rendering(
            self.module_config.clone(),
            Arc::clone(&window),
            size.width,
            size.height,
        )
        .expect("rendering host setup failed");

        self.host = Some(host);
        window.request_redraw();
        self.window = Some(window);
    }

    /// Route lifecycle and drawing events to host-owned rendering state.
    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(host) = &mut self.host {
                    host.resize(size.width, size.height);
                }
            }
            WindowEvent::RedrawRequested => self.redraw(event_loop),
            _ => {}
        }
    }
}

#[cfg(feature = "rendering")]
impl WindowedApplication {
    /// Advance, present, report statistics, and schedule the next redraw.
    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        let (Some(window), Some(host)) = (&self.window, &mut self.host) else {
            return;
        };

        match host.run_one_frame() {
            Ok(report) => {
                if let Some(report) = report {
                    println!(
                        "  {:>6.0} FPS | {:>5} entities",
                        report.fps, report.entity_count
                    );
                    window.set_title(&format!(
                        "ECS Standalone Host — {:.0} FPS | {} entities",
                        report.fps, report.entity_count
                    ));
                }
                window.request_redraw();
            }
            Err(error) => {
                eprintln!("[render] Fatal renderer error: {error}");
                event_loop.exit();
            }
        }
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// Run the configured game continuously without creating a native window.
#[cfg(not(feature = "rendering"))]
pub fn run(module_config: GameModuleConfig) -> Result<(), Box<dyn std::error::Error>> {
    let mut host = crate::setup(module_config)?;

    // Headless execution advances continuously; the engine scheduler decides
    // how each frame's systems are parallelized.
    loop {
        if let Some(report) = crate::run_one_frame(&mut host) {
            println!(
                "  {:>6.0} FPS | {:>5} entities",
                report.fps, report.entity_count
            );
        }
    }
}

/// Run the configured game in the host-owned native window and render loop.
#[cfg(feature = "rendering")]
pub fn run(module_config: GameModuleConfig) -> Result<(), Box<dyn std::error::Error>> {
    use winit::event_loop::{ControlFlow, EventLoop};

    // Poll continuously so presentation is not artificially restricted by a
    // wait-based event-loop cadence.
    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Poll);

    let mut application = WindowedApplication {
        module_config,
        window: None,
        host: None,
    };
    event_loop.run_app(&mut application)?;
    Ok(())
}
