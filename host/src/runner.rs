//! Complete standalone application runner owned by the host crate.
//!
//! The non-rendering build drives frames in a tight headless loop.
//! The rendering build owns `winit`, creates the native window,
//! asks host setup to attach the engine renderer,
//! and forwards resize/redraw events.
//!
//! # Responsibilities
//!
//! - Run the configured project in a headless loop when rendering is disabled.
//! - Run the configured project in a `winit` window when rendering is enabled.
//!
//! # Design
//!
//! The headless path drives frames directly through [`crate::run_one_frame`]
//! in an unconditional loop. The windowed path (rendering builds only) owns
//! the `winit` event loop and defers window-creation and host-setup failures
//! until after the loop exits. Embedding frontends can reuse [`crate::setup`]
//! and `setup_rendering` instead of [`run`] to supply their own window and
//! event loop.

// Standard library
#[cfg(feature = "rendering")]
use std::sync::Arc;

// External crates
#[cfg(feature = "rendering")]
use pill_core::error::FrontendError;
#[cfg(not(feature = "rendering"))]
use pill_core::error::HostError;
#[cfg(feature = "rendering")]
use pill_engine::EngineError;
#[cfg(feature = "rendering")]
use winit::application::ApplicationHandler;
#[cfg(feature = "rendering")]
use winit::event::WindowEvent;
#[cfg(feature = "rendering")]
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
#[cfg(feature = "rendering")]
use winit::window::{Window, WindowId};

// Current crate
use crate::{FrameReport, ProjectModuleConfig};

// =============================================================================
// WindowedApplication
// =============================================================================

/// State retained by `winit` for the lifetime of the standalone application.
///
/// Owns the configured project, the native window, and the rendering host, and
/// defers window-creation and host-setup failures until the loop exits so
/// they can be surfaced through [`run`]'s error path.
#[cfg(feature = "rendering")]
struct WindowedApplication {
    module_config: ProjectModuleConfig,
    window: Option<Arc<Window>>,
    host: Option<crate::RenderingHost>,
    /// Failure recorded during `resumed`; surfaced after the loop exits.
    setup_error: Option<EngineError>,
}

#[cfg(feature = "rendering")]
impl ApplicationHandler for WindowedApplication {
    /// Create the native window and complete host/renderer setup on resume.
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() || self.setup_error.is_some() {
            return;
        }

        // Step 1: Create the native window for the standalone host.
        //
        // `winit` only permits creating a window while the event loop is active,
        // so this is the first point where rendering setup can finish.
        let attributes = Window::default_attributes()
            .with_title("ECS Standalone Host")
            .with_inner_size(winit::dpi::LogicalSize::new(800.0, 600.0));
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(source) => {
                self.setup_error = Some(FrontendError::WindowCreation { source }.into());
                event_loop.exit();
                return;
            }
        };
        let size = window.inner_size();

        // Step 2: Complete host setup with rendering enabled, attaching the engine renderer
        // to the native window and configuring the initial viewport size.
        //
        // Renderer construction is part of host setup; this runner supplies
        // only the platform window that wgpu needs for its surface.
        match crate::setup_rendering(
            self.module_config.clone(),
            Arc::clone(&window),
            size.width,
            size.height,
        ) {
            Ok(host) => {
                // Step 3: Store the host and window for use in the event loop.
                self.host = Some(host);
                window.request_redraw();
                self.window = Some(window);
            }
            Err(error) => {
                self.setup_error = Some(error);
                event_loop.exit();
            }
        }
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
        // Step 1: Return early until the window and host have finished setup.
        let (Some(window), Some(host)) = (&self.window, &mut self.host) else {
            return;
        };

        // Step 2: Advance simulation and rendering by a single frame.
        match host.run_one_frame() {
            Ok(report) => {
                // Step 3: Publish frame statistics and schedule the next redraw.
                if let Some(report) = report {
                    print_frame_statistics(&report);
                    window.set_title(&format!(
                        "ECS Standalone Host — {:.0} FPS | {} entities",
                        report.fps, report.entity_count
                    ));
                }
                window.request_redraw();
            }
            Err(source) => {
                // Step 4: The frame renderer failed; stop the loop and report
                // the typed failure through the regular error boundary.
                self.setup_error = Some(source.into());
                event_loop.exit();
            }
        }
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// Run the configured project continuously without creating a native window.
///
/// # Errors
///
/// Returns [`HostError`] if host setup fails, such as when the project module
/// cannot be built or loaded, or when the source watcher cannot start. Frame
/// execution never returns an error; the loop runs until the process exits.
#[cfg(not(feature = "rendering"))]
pub fn run(module_config: ProjectModuleConfig) -> Result<(), HostError> {
    let mut host = crate::setup(module_config)?;

    loop {
        if let Some(report) = crate::run_one_frame(&mut host) {
            print_frame_statistics(&report);
        }
    }
}

/// Run the configured project in the host-owned native window and render loop.
///
/// # Errors
///
/// Returns [`EngineError`] if the event loop cannot be created or run, or if
/// window creation or host/renderer setup fails inside the event loop.
#[cfg(feature = "rendering")]
pub fn run(module_config: ProjectModuleConfig) -> Result<(), EngineError> {
    // Step 1: Create a new event loop for the windowed application.
    let event_loop =
        EventLoop::new().map_err(|source| FrontendError::EventLoopCreation { source })?;

    // Step 2: Poll continuously so the host runs frames as fast as possible
    // without waiting for user input.
    event_loop.set_control_flow(ControlFlow::Poll);

    // Step 3: Create the application state and run the event loop.
    let mut application = WindowedApplication {
        module_config,
        window: None,
        host: None,
        setup_error: None,
    };

    // Step 4: Run the event loop until the window is closed.
    event_loop
        .run_app(&mut application)
        .map_err(|source| FrontendError::EventLoopCreation { source })?;

    // Window-creation and host setup happen inside the event loop; surface
    // any deferred failure after the loop exits.
    if let Some(error) = application.setup_error {
        return Err(error);
    }
    Ok(())
}

/// Print one frame's statistics to the host console.
fn print_frame_statistics(report: &FrameReport) {
    println!(
        "  {:>6.0} FPS | {:>5} entities",
        report.fps, report.entity_count
    );
}
