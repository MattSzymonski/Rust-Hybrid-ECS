//! The windowed run loop: a `winit` window, the renderer on it, and one frame
//! per redraw.
//!
//! # Responsibilities
//!
//! - Run the project in a `winit` window: create the window, attach the
//!   renderer, and forward window and device events.
//! - Feed the run's input to the engine: keyboard and mouse from winit,
//!   gamepads polled before each frame, and rumble played after it.
//! - In a development build, run the two optional test frontends: a second
//!   renderer surface (`PILL_SECOND_VIEW=1`, [`SecondView`]) and a picking
//!   probe (`PILL_PICK_TEST=1`, [`PickTest`]).
//! - Tear the run down between two log lines, so a crash inside the teardown
//!   is visible in the log.
//!
//! # Design
//!
//! The loop is written against [`FrameDriver`], so the same code runs the
//! development host and a shipped game; [`crate::posture`] is the only module
//! that names which posture a build runs. Window-creation and setup failures
//! are deferred until the event loop exits, reported where they happen first,
//! and input is forwarded through [`FrameDriver::push_input`] as `pill_input`
//! translates it, the same translation the web uses.
//!
//! This file is compiled only in a windowed build: `main.rs` declares it
//! instead of `headless_standalone` while the `rendering` feature is on, which
//! is why nothing here repeats that feature on each item.

// Standard library
use std::sync::Arc;

// External crates
#[cfg(feature = "development")]
use pill_core::error;
use pill_core::info;
use pill_core::telemetry::telemetry_target;
use pill_input::gamepads::Gamepads;
use pill_input::winit_events::{translate_device_event, translate_window_event};
use pill_player::{report_failure, FrameDriver, WindowedFrameDriver};
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

// Current crate
use crate::error::{FrontendError, RunError};
use crate::posture;

// =============================================================================
// Constants
// =============================================================================

/// Environment variable that opens a second window drawn as surface 1.
#[cfg(feature = "development")]
const SECOND_VIEW_ENVIRONMENT_VARIABLE: &str = "PILL_SECOND_VIEW";

/// Environment variable that closes the second window after that many seconds.
#[cfg(feature = "development")]
const SECOND_VIEW_SECONDS_ENVIRONMENT_VARIABLE: &str = "PILL_SECOND_VIEW_SECONDS";

/// The second view's surface id.
#[cfg(feature = "development")]
const SECOND_VIEW_SURFACE: pill_player::SurfaceId = pill_player::SurfaceId(1);

/// Environment variable that runs the picking test.
#[cfg(feature = "development")]
const PICK_TEST_ENVIRONMENT_VARIABLE: &str = "PILL_PICK_TEST";

/// Frames the picking test lets the scene settle before it picks.
#[cfg(feature = "development")]
const PICK_TEST_SETTLE_FRAMES: u32 = 30;

// =============================================================================
// SecondView
// =============================================================================

/// The second renderer surface: one scene drawn through a second camera in a
/// second window.
///
/// Its camera is a `CameraBinding::View`, plain data rather than a world
/// entity, so the second view needs nothing in the project's world.
#[cfg(feature = "development")]
struct SecondView {
    /// The second window, kept alive while surface 1 draws on it.
    window: Arc<Window>,
    /// When to close it, if `PILL_SECOND_VIEW_SECONDS` asked.
    close_at: Option<pill_core::platform::Instant>,
}

// =============================================================================
// PickTest
// =============================================================================

/// The picking test frontend: once the scene has settled, ask what is under
/// the main window's center and a pixel inside its top-left corner, log both
/// answers, and outline the entity the center hit.
#[cfg(feature = "development")]
#[derive(Default)]
struct PickTest {
    /// Frames drawn since the test started.
    frames: u32,
    /// The center and corner request ids, once asked.
    requests: Option<(u64, u64)>,
    /// The answers that arrived, by request.
    center: Option<pill_player::PickHit>,
    corner: Option<pill_player::PickHit>,
    /// Whether the test has stopped, after logging both answers or after
    /// giving up.
    finished: bool,
}

#[cfg(feature = "development")]
impl PickTest {
    /// Advance the test by one drawn frame of a `window_width` by
    /// `window_height` window.
    fn step(
        &mut self,
        driver: &mut posture::WindowedDriver,
        window_width: u32,
        window_height: u32,
    ) {
        if self.finished {
            return;
        }
        self.frames += 1;
        let world = driver.host_mut().engine_mut().world_mut();
        // Step 1: Ask once the scene has settled: the center and a pixel just
        // inside the top-left corner, both on the primary surface.
        let Some((center_id, corner_id)) = self.requests else {
            if self.frames < PICK_TEST_SETTLE_FRAMES {
                return;
            }
            let Some(requests) = world.get_resource_mut::<pill_player::PickRequests>() else {
                error!(target: telemetry_target::ENGINE, "[pick test] the world has no PickRequests");
                self.finished = true;
                return;
            };
            let surface = pill_player::SurfaceId::PRIMARY;
            let center = requests.request(surface, window_width / 2, window_height / 2);
            let corner = requests.request(surface, 1, 1);
            self.requests = Some((center, corner));
            info!(
                target: telemetry_target::ENGINE,
                "[pick test] requested center ({}, {}) and corner (1, 1)",
                window_width / 2,
                window_height / 2
            );
            return;
        };
        // Step 2: Take the answers that arrived.
        if let Some(results) = world.get_resource_mut::<pill_player::PickResults>() {
            for result in results.take() {
                if result.request.id == center_id {
                    self.center = Some(result.hit);
                } else if result.request.id == corner_id {
                    self.corner = Some(result.hit);
                }
            }
        }
        let (Some(center), Some(corner)) = (self.center, self.corner) else {
            return;
        };
        // Step 3: With both in, log them and outline the entity the center hit.
        self.finished = true;
        info!(target: telemetry_target::ENGINE, "[pick test] center: {center:?}");
        info!(target: telemetry_target::ENGINE, "[pick test] corner: {corner:?}");
        if let pill_player::PickHit::Entity(entity) = center {
            if let Some(outlines) = world.get_resource_mut::<pill_player::SelectionOutlines>() {
                outlines.entities = vec![entity];
                info!(target: telemetry_target::ENGINE, "[pick test] outlined {entity:?}");
            }
        }
    }
}

// =============================================================================
// WindowedApplication
// =============================================================================

/// State retained by `winit` for the lifetime of the standalone application.
///
/// Owns the configured project, the native window and the windowed driver, and
/// records a window-creation or setup failure for [`run`] to return once the
/// event loop has exited.
struct WindowedApplication {
    project: posture::Project,
    /// The project running with the renderer attached: the development host or
    /// the shipped runtime, whichever this build compiled.
    driver: Option<posture::WindowedDriver>,
    window: Option<Arc<Window>>,
    /// Whether the hidden startup window has been revealed after its first frame.
    window_shown: bool,
    /// The first failure recorded inside the loop; surfaced after it exits.
    setup_error: Option<RunError>,
    /// The gamepads, polled before every frame.
    gamepads: Gamepads,
    /// The second window, when `PILL_SECOND_VIEW=1` asked for one.
    #[cfg(feature = "development")]
    second_view: Option<SecondView>,
    /// The picking test, when `PILL_PICK_TEST=1` asked for it.
    #[cfg(feature = "development")]
    pick_test: Option<PickTest>,
}

impl ApplicationHandler for WindowedApplication {
    /// Create the native window and complete setup on resume.
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() || self.setup_error.is_some() {
            return;
        }

        // Step 1: Set the project up before creating any window: the first
        // development launch compiles the game, which takes tens of seconds,
        // and a window created first would show a blank surface for all of it.
        // `winit` only allows a window while the event loop is active, which is
        // why setup cannot run before `resumed`.
        let driver = match posture::setup(self.project.clone()) {
            Ok(driver) => driver,
            Err(error) => {
                report_failure("host setup", &error);
                self.setup_error = Some(error.into());
                event_loop.exit();
                return;
            }
        };

        // Step 2: Create the native window, hidden. `winit` exposes no
        // client-area background color to set instead, so a window revealed
        // only after its first frame never shows the OS default white surface.
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

        // Step 3: Attach the renderer to the native window, which is already
        // set up, so the surface opens on a live world instead of a blank one.
        match posture::attach_renderer(driver, Arc::clone(&window), size.width, size.height) {
            Ok(driver) => {
                // Store the driver and window, then present one frame while the
                // window is still hidden and reveal it.
                self.driver = Some(driver);
                self.window = Some(window);
                self.present_first_frame_and_reveal(event_loop);
                #[cfg(feature = "development")]
                self.open_second_view_if_requested(event_loop);
                #[cfg(feature = "development")]
                if std::env::var(PICK_TEST_ENVIRONMENT_VARIABLE).as_deref() == Ok("1") {
                    self.pick_test = Some(PickTest::default());
                }
            }
            Err(error) => {
                report_failure("renderer attachment", &error);
                self.setup_error = Some(error.into());
                event_loop.exit();
            }
        }
    }

    /// Route lifecycle, drawing and input events to the windowed driver.
    ///
    /// An event of the second window goes to `second_view_event` instead.
    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        #[cfg(feature = "development")]
        if self
            .second_view
            .as_ref()
            .is_some_and(|view| view.window.id() == window_id)
        {
            self.second_view_event(event);
            return;
        }
        #[cfg(not(feature = "development"))]
        let _ = window_id;
        for input in translate_window_event(&event) {
            if let Some(driver) = &mut self.driver {
                driver.push_input(input);
            }
        }
        match event {
            WindowEvent::CloseRequested => {
                // The loop ends here as well as on a frame failure, so the close
                // gets a line of its own: it is what tells a clean shutdown apart
                // from a loop that stopped because a frame could not be drawn.
                info!(target: telemetry_target::ENGINE, "Close requested; leaving the event loop");
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(driver) = &mut self.driver {
                    WindowedFrameDriver::resize_surface(
                        driver,
                        pill_player::SurfaceId::PRIMARY,
                        size.width,
                        size.height,
                    );
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
        if let (Some(input), Some(driver)) = (translate_device_event(&event), &mut self.driver) {
            driver.push_input(input);
        }
    }
}

#[cfg(feature = "development")]
impl WindowedApplication {
    /// Open the second window as surface 1 when `PILL_SECOND_VIEW=1`, with a
    /// camera of its own; a failure is reported and the run goes on without it.
    fn open_second_view_if_requested(&mut self, event_loop: &ActiveEventLoop) {
        if std::env::var(SECOND_VIEW_ENVIRONMENT_VARIABLE).as_deref() != Ok("1") {
            return;
        }
        let Some(driver) = self.driver.as_mut() else {
            return;
        };
        let attributes = Window::default_attributes()
            .with_title(format!("{} - second view", self.project.name))
            .with_inner_size(winit::dpi::LogicalSize::new(640.0, 400.0));
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(source) => {
                report_failure("second view window creation", &source);
                return;
            }
        };
        let size = window.inner_size();
        if let Err(error) = driver.attach_surface(
            SECOND_VIEW_SURFACE,
            Arc::clone(&window),
            size.width,
            size.height,
            pill_player::SurfaceOptions::default(),
        ) {
            report_failure("second view surface", &error);
            return;
        }
        // The same scene from higher up and further back.
        driver.set_surface_camera(
            SECOND_VIEW_SURFACE,
            Some(pill_player::CameraBinding::View {
                camera: pill_player::CameraComponent::default(),
                transform: pill_player::TransformComponent {
                    translation: [0.0, 3.0, 16.0],
                    ..Default::default()
                },
            }),
        );
        let close_at = std::env::var(SECOND_VIEW_SECONDS_ENVIRONMENT_VARIABLE)
            .ok()
            .and_then(|value| value.trim().parse::<f64>().ok())
            .map(|seconds| {
                pill_core::platform::Instant::now() + std::time::Duration::from_secs_f64(seconds)
            });
        info!(
            target: telemetry_target::ENGINE,
            "[second view] opened as surface {} ({}x{})",
            SECOND_VIEW_SURFACE.0,
            size.width,
            size.height
        );
        self.second_view = Some(SecondView { window, close_at });
    }

    /// Route one event of the second window: close and resize; its redraws
    /// ride the main window's frames.
    fn second_view_event(&mut self, event: WindowEvent) {
        let Some(driver) = self.driver.as_mut() else {
            return;
        };
        match event {
            WindowEvent::CloseRequested => {
                close_second_view_of(driver, &mut self.second_view, "its window was closed");
            }
            WindowEvent::Resized(size) => {
                driver.resize_surface(SECOND_VIEW_SURFACE, size.width, size.height);
            }
            _ => {}
        }
    }
}

impl WindowedApplication {
    /// Present one frame while the window is still hidden, then reveal it.
    ///
    /// The window opens holding a rendered frame, so the OS default white
    /// startup background is never visible: `winit` exposes no client-area
    /// background color to set instead. The frame is presented synchronously
    /// because a hidden window never receives redraw requests on Windows.
    fn present_first_frame_and_reveal(&mut self, event_loop: &ActiveEventLoop) {
        // Step 1: Do nothing until the window and driver are ready.
        let Some(window) = self.window.clone() else {
            return;
        };
        let Some(driver) = self.driver.as_mut() else {
            return;
        };

        // Step 2: Present one frame to the hidden window's surface, then either
        // reveal the window already holding it or report the failure and leave
        // the window hidden.
        match driver.run_frame() {
            Ok(_) => {
                window.set_visible(true);
                self.window_shown = true;
                window.request_redraw();
                info!(target: telemetry_target::ENGINE, "First frame presented; window shown");
            }
            Err(source) => {
                report_failure("first frame render", &source);
                self.setup_error = Some(source.into());
                event_loop.exit();
            }
        }
    }

    /// Advance and present one update, then schedule the next redraw.
    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        // Step 1: Do nothing until the window and driver have finished setup.
        let (Some(window), Some(driver)) = (&self.window, &mut self.driver) else {
            return;
        };

        // Step 2: Queue what happened since the last frame: the gamepad changes
        // (keyboard and mouse arrived as window events already) and, in a
        // development build, closing the second window once its deadline passed.
        self.gamepads.poll(|input| driver.push_input(input));
        #[cfg(feature = "development")]
        let close_second_view = self.second_view.as_ref().is_some_and(|view| {
            view.close_at
                .is_some_and(|close_at| pill_core::platform::Instant::now() >= close_at)
        });
        #[cfg(feature = "development")]
        if close_second_view {
            close_second_view_of(driver, &mut self.second_view, "its time is up");
        }

        // Step 3: Advance simulation and rendering by one frame, play the
        // rumble its systems asked for, and let the picking test read the frame
        // it just drew.
        match driver.run_frame() {
            Ok(()) => {
                self.gamepads.play_rumble(driver.take_rumble_requests());
                #[cfg(feature = "development")]
                if let Some(pick_test) = &mut self.pick_test {
                    let size = window.inner_size();
                    pick_test.step(driver, size.width, size.height);
                }

                // Step 4: Reveal the window again if a platform recreated it
                // after setup, then schedule the next redraw. The window title
                // stays the project name; the frame statistics are logged by
                // the runtime itself, on the project's own log settings.
                if !self.window_shown {
                    window.set_visible(true);
                    self.window_shown = true;
                }
                window.request_redraw();
            }
            Err(source) => {
                // The renderer failed on this frame: report where it happens and
                // stop the loop.
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

/// Detach surface 1 and drop its window; the main window keeps drawing.
#[cfg(feature = "development")]
fn close_second_view_of(
    driver: &mut posture::WindowedDriver,
    second_view: &mut Option<SecondView>,
    reason: &str,
) {
    if second_view.is_none() {
        return;
    }
    // The surface first, then the window it draws on.
    driver.detach_surface(SECOND_VIEW_SURFACE);
    driver.set_surface_camera(SECOND_VIEW_SURFACE, None);
    *second_view = None;
    info!(
        target: telemetry_target::ENGINE,
        "[second view] closed: {reason}"
    );
}

/// Drop a frontend's state, announcing the teardown around it.
///
/// The drop unmaps every module copy (in development) and drops the engine,
/// and it is the one phase where the process can die with nothing of its own
/// to say: a call into an already-unmapped image faults natively. The two lines
/// bracket that region in the log, so a crash inside the drop shows the first
/// line and no second one.
fn teardown<T>(state: T) {
    info!(target: telemetry_target::ENGINE, "Shutting down");
    drop(state);
    info!(target: telemetry_target::ENGINE, "Shutdown complete");
}

/// Run the project in a native window.
///
/// # Errors
///
/// Returns [`RunError`] if the event loop cannot be created or run, or if
/// window creation, setup or the renderer fails inside the event loop.
pub fn run(project: posture::Project) -> Result<(), RunError> {
    // Step 1: Create the event loop, polling continuously so frames run as
    // fast as possible without waiting for user input.
    let event_loop =
        EventLoop::new().map_err(|source| FrontendError::EventLoopCreation { source })?;
    event_loop.set_control_flow(ControlFlow::Poll);

    // Step 2: Build the application state around the project.
    let mut application = WindowedApplication {
        project,
        window: None,
        driver: None,
        window_shown: false,
        setup_error: None,
        gamepads: Gamepads::new(),
        #[cfg(feature = "development")]
        second_view: None,
        #[cfg(feature = "development")]
        pick_test: None,
    };

    // Step 3: Run the event loop until the window is closed, then tear the
    // application down holding the failure it recorded. That failure is
    // deliberately not reported here: every path that records one reports it
    // where it happens, before this teardown, because the drop unmaps the
    // module images and a fault inside it would end the process before a report
    // made here could reach the console.
    let event_loop_result = event_loop.run_app(&mut application);
    let deferred_error = application.setup_error.take();
    teardown(application);

    // Step 4: Surface the first failure, so the event loop's own error cannot
    // mask the deferred one that explains it.
    if let Some(error) = deferred_error {
        return Err(error);
    }
    event_loop_result.map_err(|source| FrontendError::EventLoopCreation { source })?;
    Ok(())
}
