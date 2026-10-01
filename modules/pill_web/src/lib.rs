//! The browser frontend: a statically linked project running in a page's canvas.
//!
//! # Responsibilities
//!
//! - Start a shipped project ([`run`]): readable panics, console logging, and
//!   the project's modules registered through `pill_runtime`.
//! - Put a `winit` window on the page's canvas, attach the renderer to it
//!   asynchronously, and run one frame per browser animation frame.
//! - Feed the canvas's keyboard and mouse events to the engine. The canvas is
//!   focusable and focused at start, so keys reach the game without a click.
//!
//! # Design
//!
//! The web counterpart of `pill_standalone`'s shipping posture, over the same
//! `pill_runtime` and [`FrameDriver`]. Two things differ, both because a
//! browser only makes progress once control returns to it: the event loop is
//! handed over with `spawn_app` rather than run, and the renderer attach - a
//! future - is spawned onto that loop instead of blocked on. Frames start
//! once it resolves.
//!
//! The crate is empty on every other target: its dependencies are wasm-only,
//! so the workspace's native builds do not see it.

#![cfg(target_arch = "wasm32")]

// Standard library
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

// External crates
use pill_core::telemetry::telemetry_target;
use pill_core::utils::format_error_chain;
use pill_core::{error, info, warn};
use pill_input::winit_events::{translate_device_event, translate_window_event};
use pill_runtime::{FrameDriver, RenderingRuntime, Runtime, StaticProject};
use wasm_bindgen::JsCast;
use web_sys::HtmlCanvasElement;
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::platform::web::{EventLoopExtWebSys, WindowAttributesExtWebSys};
use winit::window::{Window, WindowId};

/// Run `project` in the page's canvas whose element id is `canvas_id`.
///
/// Returns once the event loop is handed to the browser; the game then runs on
/// its animation frames. Every failure is reported to the browser console.
pub fn run(project: StaticProject, canvas_id: &str) {
    // Step 1: Readable panics and console logging, before anything can fail.
    console_error_panic_hook::set_once();
    if let Err(error) = pill_runtime::init_telemetry(None) {
        web_sys::console::error_1(&format!("[web] telemetry failed to start: {error}").into());
    }

    // Step 2: Register the linked modules and the project.
    let runtime = match pill_runtime::setup(project) {
        Ok(runtime) => runtime,
        Err(error) => return report_failure("project setup", &error),
    };

    // Step 3: Hand the event loop to the browser; the canvas window and the
    // renderer follow in `resumed`.
    let event_loop = match EventLoop::new() {
        Ok(event_loop) => event_loop,
        Err(error) => return report_failure("event loop creation", &error),
    };
    info!(target: telemetry_target::ENGINE, canvas = canvas_id, "starting in the browser");
    event_loop.spawn_app(WebApplication {
        canvas_id: canvas_id.to_owned(),
        runtime: Some(runtime),
        window: None,
        driver: Rc::new(RefCell::new(None)),
    });
}

// =============================================================================
// WebApplication
// =============================================================================

/// State `winit` keeps for the lifetime of the page.
struct WebApplication {
    /// The element id of the canvas to draw on.
    canvas_id: String,
    /// The started project, until the canvas window exists to attach it to.
    runtime: Option<Runtime>,
    /// The window on the canvas.
    window: Option<Arc<Window>>,
    /// The windowed driver, filled in when the asynchronous attach resolves.
    /// Shared with that future, which runs on the browser's event loop.
    driver: Rc<RefCell<Option<RenderingRuntime>>>,
}

impl ApplicationHandler for WebApplication {
    /// Create the canvas window and start attaching the renderer to it.
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // Step 1: Only once; a later resume keeps the existing window.
        let Some(runtime) = self.runtime.take() else {
            return;
        };

        // Step 2: Put a window on the page's canvas.
        let canvas = match find_canvas(&self.canvas_id) {
            Ok(canvas) => canvas,
            Err(message) => {
                error!(target: telemetry_target::ENGINE, "{message}");
                return;
            }
        };
        // The canvas as the page laid it out, in physical pixels: winit only
        // measures it once its resize observer reports, which can be after
        // the attach below.
        let laid_out = physical_size(&canvas);
        // Keyboard events only reach an element that can hold focus. Focusing
        // it now spares the player a click before the first key works.
        let focus_target = canvas.clone();
        let attributes = Window::default_attributes()
            .with_canvas(Some(canvas))
            .with_focusable(true);
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(error) => return report_failure("canvas window creation", &error),
        };
        if let Err(error) = focus_target.focus() {
            warn!(
                target: telemetry_target::ENGINE,
                error = ?error,
                "could not focus the canvas; click it to send keys"
            );
        }
        self.window = Some(Arc::clone(&window));

        // Step 3: Attach the renderer without blocking: the device is created
        // by the browser, which only answers once control returns to it.
        // WebGPU refuses a zero-sized surface, so a canvas winit has not
        // measured yet is attached at its laid-out size.
        let size = window.inner_size();
        let (width, height) = if size.width > 0 && size.height > 0 {
            (size.width, size.height)
        } else {
            laid_out
        };
        let driver = Rc::clone(&self.driver);
        pill_core::platform::futures::spawn(async move {
            let attach = pill_runtime::attach_renderer(runtime, Arc::clone(&window), width, height);
            match attach.await {
                Ok(mut attached) => {
                    // A resize that arrived during the attach went nowhere;
                    // apply the canvas's size as winit now measures it. Not
                    // an unmeasured zero, which would read as minimized.
                    let size = window.inner_size();
                    if size.width > 0
                        && size.height > 0
                        && (size.width, size.height) != (width, height)
                    {
                        attached.resize(size.width, size.height);
                    }
                    *driver.borrow_mut() = Some(attached);
                    info!(target: telemetry_target::RENDERING, "renderer attached to the canvas");
                    window.request_redraw();
                }
                Err(error) => report_failure("renderer attachment", &error),
            }
        });
    }

    /// Forward input and resizes to the driver and run a frame per redraw.
    ///
    /// Input that arrives before the renderer is attached has no game to go to
    /// and is dropped.
    fn window_event(&mut self, _event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        if let Some(input) = translate_window_event(&event) {
            if let Some(driver) = self.driver.borrow_mut().as_mut() {
                driver.push_input(input);
            }
        }
        match event {
            WindowEvent::Resized(size) => {
                if let Some(driver) = self.driver.borrow_mut().as_mut() {
                    driver.resize(size.width, size.height);
                }
            }
            WindowEvent::RedrawRequested => self.redraw(),
            _ => {}
        }
    }

    /// Forward raw mouse motion, reported per device rather than per window.
    fn device_event(&mut self, _event_loop: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        if let Some(input) = translate_device_event(&event) {
            if let Some(driver) = self.driver.borrow_mut().as_mut() {
                driver.push_input(input);
            }
        }
    }
}

impl WebApplication {
    /// Run one frame and ask the browser for the next one.
    ///
    /// Does nothing until the renderer is attached. A failed frame is reported
    /// and no further frame is requested, so the page stops on the error
    /// instead of repeating it every frame.
    fn redraw(&mut self) {
        let Some(window) = &self.window else {
            return;
        };
        let mut driver = self.driver.borrow_mut();
        let Some(driver) = driver.as_mut() else {
            return;
        };
        match driver.run_frame() {
            Ok(report) => {
                if let Some(report) = report {
                    info!(
                        target: telemetry_target::ENGINE,
                        fps = report.fps,
                        entities = report.entity_count,
                        "frame statistics"
                    );
                }
                window.request_redraw();
            }
            Err(error) => report_failure("frame render", &error),
        }
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// The page's canvas element with id `canvas_id`.
///
/// # Errors
///
/// Returns a description of what is missing: the document, the element, or
/// an element that is not a canvas.
fn find_canvas(canvas_id: &str) -> Result<HtmlCanvasElement, String> {
    let document = web_sys::window()
        .and_then(|window| window.document())
        .ok_or("the page has no document")?;
    let element = document
        .get_element_by_id(canvas_id)
        .ok_or_else(|| format!("the page has no element with id `{canvas_id}`"))?;
    element
        .dyn_into::<HtmlCanvasElement>()
        .map_err(|_| format!("the element `{canvas_id}` is not a canvas"))
}

/// The canvas's laid-out size in physical pixels, at least 1x1.
fn physical_size(canvas: &HtmlCanvasElement) -> (u32, u32) {
    let ratio = web_sys::window().map_or(1.0, |window| window.device_pixel_ratio());
    let scale = |css_pixels: i32| (f64::from(css_pixels) * ratio).round().max(1.0) as u32;
    (scale(canvas.client_width()), scale(canvas.client_height()))
}

/// Report a failure to the log, with its whole cause chain.
fn report_failure(context: &str, error: &(dyn std::error::Error + 'static)) {
    let cause_chain = format_error_chain(error);
    error!(target: telemetry_target::ENGINE, error = %cause_chain, "{} failed", context);
}
