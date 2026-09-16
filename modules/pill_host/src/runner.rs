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
use pill_core::error;
#[cfg(not(feature = "rendering"))]
use pill_core::error::HostError;
#[cfg(feature = "rendering")]
use pill_core::telemetry::telemetry_target;
#[cfg(feature = "rendering")]
use pill_core::utils::format_error_chain;
#[cfg(feature = "rendering")]
use winit::application::ApplicationHandler;
#[cfg(feature = "rendering")]
use winit::event::WindowEvent;
#[cfg(feature = "rendering")]
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
#[cfg(feature = "rendering")]
use winit::window::{Window, WindowId};

// Current crate
#[cfg(feature = "rendering")]
use crate::frontend::{FrontendError, RenderingError};
use crate::FrameReport;

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
    project: crate::ProjectSource,
    window: Option<Arc<Window>>,
    host: Option<crate::RenderingHost>,
    /// Whether the hidden startup window has been revealed after its first frame.
    window_shown: bool,
    /// Failure recorded during `resumed`; surfaced after the loop exits.
    setup_error: Option<RenderingError>,
}

#[cfg(feature = "rendering")]
impl ApplicationHandler for WindowedApplication {
    /// Create the native window and complete host/renderer setup on resume.
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() || self.setup_error.is_some() {
            return;
        }

        // Step 1: Build and load the project module BEFORE creating any window.
        //
        // The first standalone launch must compile the game, which can take
        // tens of seconds. Creating the window first would show a blank white
        // surface for the entire build, so project setup runs ahead of window
        // creation. `winit` only permits creating a window while the event loop
        // is active, which is why setup cannot happen before `resumed`.
        let host = match crate::setup(self.project.clone()) {
            Ok(host) => host,
            Err(error) => {
                report_failure("host setup", &error);
                self.setup_error = Some(error.into());
                event_loop.exit();
                return;
            }
        };

        // Step 2: Create the native window for the standalone host, hidden.
        //
        // `winit` only permits creating a window while the event loop is active,
        // so this is the first point where the surface can be created. The window
        // starts invisible: winit 0.30 exposes no client-area background color,
        // so revealing it only after the first frame renders prevents the OS
        // default white surface from ever being shown.
        let attributes = Window::default_attributes()
            .with_title(self.project.name.to_owned())
            .with_inner_size(winit::dpi::LogicalSize::new(800.0, 600.0))
            .with_visible(false);
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(source) => {
                report_failure("window creation", &source);
                self.setup_error = Some(FrontendError::WindowCreation { source }.into());
                event_loop.exit();
                return;
            }
        };
        let size = window.inner_size();

        // Step 3: Attach the engine renderer to the native window and complete
        // the rendering host. The project module is already built and loaded,
        // so the surface opens on a live world instead of a blank window.
        match crate::attach_renderer(host, Arc::clone(&window), size.width, size.height) {
            Ok(host) => {
                // Step 4: Store the host and window, present the first frame
                // while the window is still hidden, then reveal it already
                // holding rendered content. See `present_first_frame_and_reveal`.
                self.host = Some(host);
                self.window = Some(window);
                self.present_first_frame_and_reveal(event_loop);
            }
            Err(error) => {
                report_failure("renderer attachment", &error);
                self.setup_error = Some(error.into());
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
            WindowEvent::CloseRequested => {
                // The loop ends here as well as in the error arms, so it gets a
                // line of its own: without it, a shutdown that follows a window
                // close looks exactly like one that follows a failure in the
                // frame path - both are an event loop that simply stopped.
                println!("[host] Close requested; leaving the event loop.");
                event_loop.exit();
            }
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
    /// Present one frame while the window is still hidden, then reveal it.
    ///
    /// Rendering before the window is shown guarantees the surface already
    /// holds the engine's black-cleared frame, so the OS default white
    /// startup background is never visible. winit 0.30 exposes no client-area
    /// background color attribute, so this is the only cross-platform way to
    /// open on a black surface. The frame is also presented synchronously
    /// because a hidden window never receives redraw requests on Windows.
    fn present_first_frame_and_reveal(&mut self, event_loop: &ActiveEventLoop) {
        // Step 1: Do nothing until the window and host are ready.
        let Some(window) = self.window.clone() else {
            return;
        };
        let Some(host) = self.host.as_mut() else {
            return;
        };

        // Step 2: Present one frame to the hidden window's surface.
        match host.run_one_frame() {
            Ok(_) => {
                // Step 3: Reveal the window now that it holds rendered content.
                window.set_visible(true);
                self.window_shown = true;
                window.request_redraw();
                println!("[host] First frame presented; window shown.");
            }
            Err(source) => {
                // Step 4: Rendering failed before the window was shown; report
                // through the regular error boundary without revealing it.
                report_failure("first frame render", &source);
                self.setup_error = Some(source.into());
                event_loop.exit();
            }
        }
    }

    /// Advance, present, report statistics, and schedule the next redraw.
    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        // Step 1: Return early until the window and host have finished setup.
        let (Some(window), Some(host)) = (&self.window, &mut self.host) else {
            return;
        };

        // Step 2: Advance simulation and rendering by a single frame.
        match host.run_one_frame() {
            Ok(report) => {
                // Step 3: Defensive reveal in case a platform recreates the
                // window after setup (setup already revealed it after the
                // first synchronous frame). The engine clears each frame to
                // black, so the window never shows the OS default white
                // background.
                if !self.window_shown {
                    window.set_visible(true);
                    self.window_shown = true;
                }

                // Step 4: Publish frame statistics and schedule the next redraw.
                // The window title stays exactly the project name from
                // project_settings.yaml (set once at window creation); only the
                // console carries the live frame stats.
                if let Some(report) = report {
                    print_frame_statistics(&report);
                }
                window.request_redraw();
            }
            Err(source) => {
                // Step 5: The frame renderer failed; stop the loop and report
                // the typed failure through the regular error boundary.
                report_failure("frame render", &source);
                self.setup_error = Some(source.into());
                event_loop.exit();
            }
        }
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// Report a failure where it happens, before anything starts tearing down.
///
/// The error is also stored so the run function can return it, but that return
/// happens after the host has been dropped - and dropping it unmaps the module
/// images, where a stale call can kill the process first. Reporting at the
/// point of failure is what keeps the original cause in the log, and the whole
/// source chain is reported with it, because the outer message of these errors
/// names the operation and only the causes name the reason.
#[cfg(feature = "rendering")]
fn report_failure(context: &str, error: &(dyn std::error::Error + 'static)) {
    let cause_chain = format_error_chain(error);
    eprintln!("[host] {context} failed: {cause_chain}");
    error!(
        target: telemetry_target::ENGINE,
        error = %cause_chain,
        "{} failed",
        context
    );
}

/// Drop a frontend's state, announcing the teardown around it.
///
/// The drop unmaps every module copy and drops the engine, and it is the one
/// phase where the process can die with nothing of its own to say: a call into
/// an already-unmapped image faults natively. The two lines bracket that region
/// in the log, so a crash inside the drop shows the first line and no second
/// one.
///
/// Only the windowed run reaches it: the headless loop runs until the process
/// is killed, so it never tears the host down.
#[cfg(feature = "rendering")]
fn teardown<T>(state: T) {
    println!("[host] Shutting down.");
    drop(state);
    println!("[host] Shutdown complete.");
}

/// Run the configured project continuously without creating a native window.
///
/// # Errors
///
/// Returns [`HostError`] if host setup fails, such as when the project module
/// cannot be built or loaded, or when the source watcher cannot start. Frame
/// execution never returns an error; the loop runs until the process exits.
#[cfg(not(feature = "rendering"))]
pub fn run(project: impl Into<crate::ProjectSource>) -> Result<(), HostError> {
    let mut host = crate::setup(project.into())?;

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
/// Returns [`RenderingError`] if the event loop cannot be created or run, or if
/// window creation or host/renderer setup fails inside the event loop.
#[cfg(feature = "rendering")]
pub fn run(project: impl Into<crate::ProjectSource>) -> Result<(), RenderingError> {
    let project = project.into();
    // Step 1: Create a new event loop for the windowed application.
    let event_loop =
        EventLoop::new().map_err(|source| FrontendError::EventLoopCreation { source })?;

    // Step 2: Poll continuously so the host runs frames as fast as possible
    // without waiting for user input.
    event_loop.set_control_flow(ControlFlow::Poll);

    // Step 3: Create the application state and run the event loop.
    let mut application = WindowedApplication {
        project,
        window: None,
        host: None,
        window_shown: false,
        setup_error: None,
    };

    // Step 4: Run the event loop until the window is closed.
    let event_loop_result = event_loop.run_app(&mut application);

    // Step 5: Take the failure recorded inside the loop and tear the
    // application down. The error is deliberately not reported here: every
    // path that records one reports it where it happens, before this teardown,
    // because the drop below unmaps the module images and a fault inside it
    // would end the process before a report made here could reach the console.
    let deferred_error = application.setup_error.take();
    teardown(application);

    // Step 6: Surface the first failure, so the event loop's own error cannot
    // mask the deferred one that explains it.
    if let Some(error) = deferred_error {
        return Err(error);
    }
    event_loop_result.map_err(|source| FrontendError::EventLoopCreation { source })?;
    Ok(())
}

/// Print one frame's statistics to the host console.
fn print_frame_statistics(report: &FrameReport) {
    println!(
        "  {:>6.0} FPS | {:>5} entities",
        report.fps, report.entity_count
    );
}
