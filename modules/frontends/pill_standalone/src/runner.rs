//! The standalone run loops: headless, or a `winit` window.
//!
//! # Responsibilities
//!
//! - Run the project in a headless loop when rendering is disabled.
//! - Run the project in a `winit` window when rendering is enabled: create
//!   the window, attach the renderer, and forward window and device events.
//! - Feed a windowed run's input to the engine: keyboard and mouse from winit,
//!   gamepads polled before each frame, and rumble played after it.
//! - In a development build, run the two optional test frontends: a second
//!   renderer surface (`PILL_SECOND_VIEW=1`, [`SecondView`]) and a picking
//!   probe (`PILL_PICK_TEST=1`, [`PickTest`]).
//! - Tear a windowed run down between two log lines, so a crash inside the
//!   teardown is visible in the log.
//!
//! # Design
//!
//! Both loops are written against [`FrameDriver`], so the same code runs the
//! development host and a shipped game; [`crate::posture`] is the only module
//! that names which host a build runs. The windowed loop defers window-creation
//! and setup failures until the event loop exits, reporting each where it
//! happens, and forwards input through [`FrameDriver::push_input`] as
//! `pill_input` translates it, the same translation the web uses.

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
use pill_player::{FrameDriver, FrameReport};
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
// Constants
// =============================================================================

/// Environment variable that opens a second window drawn as surface 1.
#[cfg(all(feature = "rendering", feature = "dev"))]
const SECOND_VIEW_ENVIRONMENT_VARIABLE: &str = "PILL_SECOND_VIEW";

/// Environment variable that closes the second window after that many seconds.
#[cfg(all(feature = "rendering", feature = "dev"))]
const SECOND_VIEW_SECONDS_ENVIRONMENT_VARIABLE: &str = "PILL_SECOND_VIEW_SECONDS";

/// The second view's surface id.
#[cfg(all(feature = "rendering", feature = "dev"))]
const SECOND_VIEW_SURFACE: pill_player::SurfaceId = pill_player::SurfaceId(1);

/// Environment variable that runs the picking test.
#[cfg(all(feature = "rendering", feature = "dev"))]
const PICK_TEST_ENVIRONMENT_VARIABLE: &str = "PILL_PICK_TEST";

/// Frames the picking test lets the scene settle before it picks.
#[cfg(all(feature = "rendering", feature = "dev"))]
const PICK_TEST_SETTLE_FRAMES: u32 = 30;

// =============================================================================
// SecondView
// =============================================================================

/// The second renderer surface: one scene drawn through a second camera in a
/// second window.
///
/// Its camera is a `CameraBinding::View`, plain data rather than a world
/// entity, so the second view needs nothing in the project's world.
#[cfg(all(feature = "rendering", feature = "dev"))]
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
#[cfg(all(feature = "rendering", feature = "dev"))]
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

#[cfg(all(feature = "rendering", feature = "dev"))]
impl PickTest {
    /// Advance the test by one drawn frame of a `width` by `height` window.
    fn step(&mut self, host: &mut posture::Windowed, width: u32, height: u32) {
        if self.finished {
            return;
        }
        self.frames += 1;
        let world = host.host_mut().engine_mut().world_mut();
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
            let center = requests.request(surface, width / 2, height / 2);
            let corner = requests.request(surface, 1, 1);
            self.requests = Some((center, corner));
            info!(
                target: telemetry_target::ENGINE,
                "[pick test] requested center ({}, {}) and corner (1, 1)",
                width / 2,
                height / 2
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
#[cfg(feature = "rendering")]
struct WindowedApplication {
    project: posture::Project,
    host: Option<posture::Windowed>,
    window: Option<Arc<Window>>,
    /// Whether the hidden startup window has been revealed after its first frame.
    window_shown: bool,
    /// The first failure recorded inside the loop; surfaced after it exits.
    setup_error: Option<RunError>,
    /// The gamepads, polled before every frame.
    gamepads: Gamepads,
    /// The second window, when `PILL_SECOND_VIEW=1` asked for one.
    #[cfg(feature = "dev")]
    second_view: Option<SecondView>,
    /// The picking test, when `PILL_PICK_TEST=1` asked for it.
    #[cfg(feature = "dev")]
    pick_test: Option<PickTest>,
}

#[cfg(feature = "rendering")]
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
        let host = match posture::setup(self.project.clone()) {
            Ok(host) => host,
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
        match posture::attach(host, Arc::clone(&window), size.width, size.height) {
            Ok(host) => {
                // Store the driver and window, then present one frame while the
                // window is still hidden and reveal it.
                self.host = Some(host);
                self.window = Some(window);
                self.present_first_frame_and_reveal(event_loop);
                #[cfg(feature = "dev")]
                self.open_second_view_if_requested(event_loop);
                #[cfg(feature = "dev")]
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
        #[cfg(feature = "dev")]
        if self
            .second_view
            .as_ref()
            .is_some_and(|view| view.window.id() == window_id)
        {
            self.second_view_event(event);
            return;
        }
        #[cfg(not(feature = "dev"))]
        let _ = window_id;
        for input in translate_window_event(&event) {
            if let Some(host) = &mut self.host {
                host.push_input(input);
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
                if let Some(host) = &mut self.host {
                    FrameDriver::resize_surface(
                        host,
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
        if let (Some(input), Some(host)) = (translate_device_event(&event), &mut self.host) {
            host.push_input(input);
        }
    }
}

#[cfg(all(feature = "rendering", feature = "dev"))]
impl WindowedApplication {
    /// Open the second window as surface 1 when `PILL_SECOND_VIEW=1`, with a
    /// camera of its own; a failure is reported and the run goes on without it.
    fn open_second_view_if_requested(&mut self, event_loop: &ActiveEventLoop) {
        if std::env::var(SECOND_VIEW_ENVIRONMENT_VARIABLE).as_deref() != Ok("1") {
            return;
        }
        let Some(host) = self.host.as_mut() else {
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
        if let Err(error) = host.attach_surface(
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
        host.set_surface_camera(
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
        let Some(host) = self.host.as_mut() else {
            return;
        };
        match event {
            WindowEvent::CloseRequested => {
                close_second_view_of(host, &mut self.second_view, "its window was closed");
            }
            WindowEvent::Resized(size) => {
                host.resize_surface(SECOND_VIEW_SURFACE, size.width, size.height);
            }
            _ => {}
        }
    }
}

#[cfg(feature = "rendering")]
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
        let Some(host) = self.host.as_mut() else {
            return;
        };

        // Step 2: Present one frame to the hidden window's surface, then either
        // reveal the window already holding it or report the failure and leave
        // the window hidden.
        match host.run_frame() {
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

    /// Advance, present, report statistics, and schedule the next redraw.
    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        // Step 1: Do nothing until the window and driver have finished setup.
        let (Some(window), Some(host)) = (&self.window, &mut self.host) else {
            return;
        };

        // Step 2: Queue what happened since the last frame: the gamepad changes
        // (keyboard and mouse arrived as window events already) and, in a
        // development build, closing the second window once its deadline passed.
        self.gamepads.poll(|input| host.push_input(input));
        #[cfg(feature = "dev")]
        let close_second_view = self.second_view.as_ref().is_some_and(|view| {
            view.close_at
                .is_some_and(|close_at| pill_core::platform::Instant::now() >= close_at)
        });
        #[cfg(feature = "dev")]
        if close_second_view {
            close_second_view_of(host, &mut self.second_view, "its time is up");
        }

        // Step 3: Advance simulation and rendering by one frame, play the
        // rumble its systems asked for, and let the picking test read the frame
        // it just drew.
        match host.run_frame() {
            Ok(report) => {
                self.gamepads.play_rumble(host.take_rumble_requests());
                #[cfg(feature = "dev")]
                if let Some(pick_test) = &mut self.pick_test {
                    let size = window.inner_size();
                    pick_test.step(host, size.width, size.height);
                }

                // Step 4: Reveal the window again if a platform recreated it
                // after setup, publish this frame's statistics and schedule the
                // next redraw. The window title stays the project name; only
                // the console carries the live frame statistics.
                if !self.window_shown {
                    window.set_visible(true);
                    self.window_shown = true;
                }
                if let Some(report) = report {
                    print_frame_statistics(&report);
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
#[cfg(all(feature = "rendering", feature = "dev"))]
fn close_second_view_of(
    host: &mut posture::Windowed,
    second_view: &mut Option<SecondView>,
    reason: &str,
) {
    if second_view.is_none() {
        return;
    }
    // The surface first, then the window it draws on.
    host.detach_surface(SECOND_VIEW_SURFACE);
    host.set_surface_camera(SECOND_VIEW_SURFACE, None);
    *second_view = None;
    info!(
        target: telemetry_target::ENGINE,
        "[second view] closed: {reason}"
    );
}

/// Report a failure where it happens, before anything starts tearing down.
///
/// The error is also stored for [`run`] to return, where the caller has a slot
/// for it, but that return happens after the driver is dropped - and dropping
/// the development host unmaps the module images, where a stale call would kill
/// the process first. The whole source chain goes into the line, because the
/// outer message names only the operation and the causes name the reason.
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
    // Step 1: Create the event loop, polling continuously so frames run as
    // fast as possible without waiting for user input.
    let event_loop =
        EventLoop::new().map_err(|source| FrontendError::EventLoopCreation { source })?;
    event_loop.set_control_flow(ControlFlow::Poll);

    // Step 2: Build the application state around the project.
    let mut application = WindowedApplication {
        project,
        window: None,
        host: None,
        window_shown: false,
        setup_error: None,
        gamepads: Gamepads::new(),
        #[cfg(feature = "dev")]
        second_view: None,
        #[cfg(feature = "dev")]
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

/// Log one frame's statistics. `FPS |` is what the shipping smoke test waits for.
fn print_frame_statistics(report: &FrameReport) {
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ENGINE,
        "{:.0} FPS | {} entities",
        report.fps,
        report.entity_count
    );
}
