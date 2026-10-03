//! The standalone run loops: headless, or a `winit` window.
//!
//! # Responsibilities
//!
//! - Run the project in a headless loop when rendering is disabled.
//! - Run the project in a `winit` window when rendering is enabled: create
//!   the window, attach the renderer, forward resize and redraw events.
//! - Feed a windowed run's input to the engine: keyboard and mouse from winit,
//!   gamepads polled before each frame, and rumble played after it.
//!
//! # Design
//!
//! Both loops are written against [`FrameDriver`], so one loop drives the
//! development host and a shipped game alike; [`crate::posture`] is the only
//! code that knows which it is. The windowed loop defers window-creation and
//! setup failures until the event loop exits, reporting each where it happens.
//! Input reaches the engine between frames through [`FrameDriver::push_input`];
//! the translation itself is `pill_input`'s, shared with the web.

// Standard library
#[cfg(feature = "rendering")]
use std::sync::Arc;

// External crates
#[cfg(feature = "rendering")]
use pill_core::telemetry::telemetry_target;
#[cfg(feature = "rendering")]
use pill_core::utils::format_error_chain;
#[cfg(feature = "rendering")]
use pill_core::{error, info};
#[cfg(feature = "rendering")]
use pill_input::gamepads::Gamepads;
#[cfg(feature = "rendering")]
use pill_input::winit_events::{translate_device_event, translate_window_event};
use pill_runtime::{FrameDriver, FrameReport};
#[cfg(feature = "rendering")]
use winit::application::ApplicationHandler;
#[cfg(feature = "rendering")]
use winit::event::{DeviceEvent, DeviceId, WindowEvent};
#[cfg(feature = "rendering")]
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
#[cfg(feature = "rendering")]
use winit::window::{Window, WindowId};

// Current crate
#[cfg(feature = "rendering")]
use crate::frontend::FrontendError;
use crate::frontend::RunError;
use crate::posture;

// =============================================================================
// WindowedApplication
// =============================================================================

/// State retained by `winit` for the lifetime of the standalone application.
///
/// Owns the configured project, the native window, and the windowed driver,
/// and defers window-creation and setup failures until the loop exits so they
/// can be surfaced through [`run`]'s error path.
#[cfg(feature = "rendering")]
struct WindowedApplication {
    project: posture::Project,
    host: Option<posture::Windowed>,
    window: Option<Arc<Window>>,
    /// Whether the hidden startup window has been revealed after its first frame.
    window_shown: bool,
    /// Failure recorded during `resumed`; surfaced after the loop exits.
    setup_error: Option<RunError>,
    /// The gamepads, polled before every frame.
    gamepads: Gamepads,
}

#[cfg(feature = "rendering")]
impl ApplicationHandler for WindowedApplication {
    /// Create the native window and complete setup on resume.
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() || self.setup_error.is_some() {
            return;
        }

        // Step 1: Set the project up BEFORE creating any window.
        //
        // The first development launch must compile the game, which can take
        // tens of seconds. Creating the window first would show a blank white
        // surface for the entire build, so project setup runs ahead of window
        // creation. `winit` only permits creating a window while the event loop
        // is active, which is why setup cannot happen before `resumed`.
        let host = match posture::setup(self.project.clone()) {
            Ok(host) => host,
            Err(error) => {
                report_failure("host setup", &error);
                self.setup_error = Some(error.into());
                event_loop.exit();
                return;
            }
        };

        // Step 2: Create the native window, hidden.
        //
        // The window starts invisible: winit 0.30 exposes no client-area
        // background color, so revealing it only after the first frame renders
        // prevents the OS default white surface from ever being shown.
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

        // Step 3: Attach the renderer to the native window. The project is
        // already set up, so the surface opens on a live world instead of a
        // blank window.
        match posture::attach(host, Arc::clone(&window), size.width, size.height) {
            Ok(host) => {
                // Step 4: Store the driver and window, present the first frame
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

    /// Route lifecycle, drawing and input events to the windowed driver.
    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        if let (Some(input), Some(host)) = (translate_window_event(&event), &mut self.host) {
            host.push_input(input);
        }
        match event {
            WindowEvent::CloseRequested => {
                // The loop ends here as well as in the error arms, so it gets a
                // line of its own: without it, a shutdown that follows a window
                // close looks exactly like one that follows a failure in the
                // frame path - both are an event loop that simply stopped.
                info!(target: telemetry_target::ENGINE, "Close requested; leaving the event loop");
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(host) = &mut self.host {
                    FrameDriver::resize(host, size.width, size.height);
                }
            }
            WindowEvent::RedrawRequested => self.redraw(event_loop),
            _ => {}
        }
    }

    /// Forward raw mouse motion, which winit reports per device rather than
    /// per window (and only while the window has focus).
    fn device_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _device_id: DeviceId,
        event: DeviceEvent,
    ) {
        if let (Some(input), Some(host)) = (translate_device_event(&event), &mut self.host) {
            host.push_input(input);
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
        // Step 1: Do nothing until the window and driver are ready.
        let Some(window) = self.window.clone() else {
            return;
        };
        let Some(host) = self.host.as_mut() else {
            return;
        };

        // Step 2: Present one frame to the hidden window's surface.
        match host.run_frame() {
            Ok(_) => {
                // Step 3: Reveal the window now that it holds rendered content.
                window.set_visible(true);
                self.window_shown = true;
                window.request_redraw();
                info!(target: telemetry_target::ENGINE, "First frame presented; window shown");
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
        // Step 1: Return early until the window and driver have finished setup.
        let (Some(window), Some(host)) = (&self.window, &mut self.host) else {
            return;
        };

        // Step 2: Queue what the gamepads did since the last frame; keyboard
        // and mouse arrived as window events already.
        self.gamepads.poll(|input| host.push_input(input));

        // Step 3: Advance simulation and rendering by a single frame.
        match host.run_frame() {
            Ok(report) => {
                // Step 4: Play the rumble the frame's systems asked for.
                self.gamepads.play_rumble(host.take_rumble_requests());

                // Step 5: Defensive reveal in case a platform recreates the
                // window after setup (setup already revealed it after the
                // first synchronous frame).
                if !self.window_shown {
                    window.set_visible(true);
                    self.window_shown = true;
                }

                // Step 6: Publish frame statistics and schedule the next redraw.
                // The window title stays the project name; only the console
                // carries the live frame stats.
                if let Some(report) = report {
                    print_frame_statistics(&report);
                }
                window.request_redraw();
            }
            Err(source) => {
                // Step 7: The frame renderer failed; stop the loop and report
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
/// happens after the driver has been dropped - and dropping the development
/// host unmaps the module images, where a stale call can kill the process
/// first. Reporting at the point of failure is what keeps the original cause in
/// the log, and the whole source chain is reported with it, because the outer
/// message of these errors names the operation and only the causes name the
/// reason.
#[cfg(feature = "rendering")]
fn report_failure(context: &str, error: &(dyn std::error::Error + 'static)) {
    let cause_chain = format_error_chain(error);
    error!(
        target: telemetry_target::ENGINE,
        error = %cause_chain,
        "{} failed",
        context
    );
}

/// Drop a frontend's state, announcing the teardown around it.
///
/// The drop unmaps every module copy (in development) and drops the engine,
/// and it is the one phase where the process can die with nothing of its own
/// to say: a call into an already-unmapped image faults natively. The two lines
/// bracket that region in the log, so a crash inside the drop shows the first
/// line and no second one.
///
/// Only the windowed run reaches it: the headless loop runs until the process
/// is killed, so it never tears the driver down.
#[cfg(feature = "rendering")]
fn teardown<T>(state: T) {
    info!(target: telemetry_target::ENGINE, "Shutting down");
    drop(state);
    info!(target: telemetry_target::ENGINE, "Shutdown complete");
}

/// Run `driver` frame after frame, printing each report.
///
/// # Errors
///
/// Returns the driver's frame failure; a headless driver has none, so the
/// loop runs until the process exits.
#[cfg(not(feature = "rendering"))]
fn run_headless<D: FrameDriver>(mut driver: D) -> Result<(), RunError>
where
    RunError: From<D::Error>,
{
    loop {
        if let Some(report) = driver.run_frame()? {
            print_frame_statistics(&report);
        }
    }
}

/// Run the project continuously without creating a native window.
///
/// # Errors
///
/// Returns [`RunError`] if setup fails, such as when the project cannot be
/// built, loaded or initialized. A headless frame cannot fail; the loop runs
/// until the process exits.
#[cfg(not(feature = "rendering"))]
pub fn run(project: posture::Project) -> Result<(), RunError> {
    run_headless(posture::setup(project)?)
}

/// Run the project in a native window.
///
/// # Errors
///
/// Returns [`RunError`] if the event loop cannot be created or run, or if
/// window creation, setup or the renderer fails inside the event loop.
#[cfg(feature = "rendering")]
pub fn run(project: posture::Project) -> Result<(), RunError> {
    // Step 1: Create a new event loop for the windowed application.
    let event_loop =
        EventLoop::new().map_err(|source| FrontendError::EventLoopCreation { source })?;

    // Step 2: Poll continuously so frames run as fast as possible without
    // waiting for user input.
    event_loop.set_control_flow(ControlFlow::Poll);

    // Step 3: Create the application state and run the event loop.
    let mut application = WindowedApplication {
        project,
        window: None,
        host: None,
        window_shown: false,
        setup_error: None,
        gamepads: Gamepads::new(),
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

/// Log one frame's statistics. `FPS |` is what the shipping smoke test waits for.
fn print_frame_statistics(report: &FrameReport) {
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ENGINE,
        "{:.0} FPS | {} entities",
        report.fps,
        report.entity_count
    );
}
