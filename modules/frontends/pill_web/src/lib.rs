//! The browser frontend: a statically linked project running in a page's canvas.
//!
//! # Responsibilities
//!
//! - Start a shipped project ([`run`]): readable panics, console logging, the
//!   project's asset pack downloaded beside the page, and the
//!   project's modules registered through `pill_player`.
//! - Put a `winit` window on the page's canvas, attach the renderer to it
//!   asynchronously, and run one frame per browser animation frame.
//! - Feed the canvas's keyboard and mouse events to the engine. The canvas is
//!   focusable and focused at start, so keys reach the game without a click.
//!
//! # Design
//!
//! The web counterpart of `pill_standalone`'s shipping posture, over the same
//! `pill_player` and [`FrameDriver`]. Three things differ, all because a
//! browser only makes progress once control returns to it: the event loop is
//! handed over with `spawn_app` rather than run, the renderer attach - a
//! future - is spawned onto that loop instead of blocked on, and the asset
//! pack is fetched rather than embedded, since a wasm module is downloaded
//! whole. Frames start once the attach resolves.
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
use pill_player::{FrameDriver, RenderingRuntime, Runtime, StaticProject, SurfaceId};
use wasm_bindgen::JsCast;
use web_sys::HtmlCanvasElement;
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::platform::web::{EventLoopExtWebSys, WindowAttributesExtWebSys};
use winit::window::{Window, WindowId};

/// The egui editor posture: the scene on one canvas, the egui shell's UI on a
/// transparent canvas above it.
#[cfg(target_arch = "wasm32")]
pub mod editor_egui;

/// The scripted channel the editor drains (`editor_command`).
#[cfg(target_arch = "wasm32")]
pub mod scripted;

/// Run `project` in the page's canvas whose element id is `canvas_id`.
///
/// Returns once the event loop is handed to the browser; the game then runs on
/// its animation frames. Every failure is reported to the browser console.
pub fn run(project: StaticProject, canvas_id: &str) {
    let canvas_id = canvas_id.to_owned();
    start_project(project, move |runtime| {
        // Step 1: Hand the event loop to the browser; the canvas window and
        // the renderer follow in `resumed`.
        let event_loop = match EventLoop::new() {
            Ok(event_loop) => event_loop,
            Err(error) => return report_failure("event loop creation", &error),
        };
        info!(target: telemetry_target::ENGINE, canvas = %canvas_id, "starting in the browser");
        event_loop.spawn_app(WebApplication {
            canvas_id: canvas_id.clone(),
            runtime: Some(runtime),
            window: None,
            driver: Rc::new(RefCell::new(None)),
        });
    });
}

/// Start `project` and hand the running runtime to `launch`.
///
/// Readable panics and console logging come first, because everything below
/// can fail. A project with an `asset_pack_url` is a browser build whose
/// assets were kept out of the module: its pack is downloaded and
/// mounted before the project initializes, so an init that loads a `res` file
/// finds it, exactly as the embedded path does. Both runners share this, so
/// the game and editor postures cannot diverge.
pub(crate) fn start_project<F>(project: StaticProject, launch: F)
where
    F: FnOnce(Runtime) + 'static,
{
    // Step 1: Readable panics and console logging, before anything can fail.
    console_error_panic_hook::set_once();
    if let Err(error) = pill_player::init_telemetry(None) {
        web_sys::console::error_1(&format!("[web] telemetry failed to start: {error}").into());
    }

    // Step 2: A project without a pack URL carries its assets in the module.
    let Some(url) = project.asset_pack_url else {
        match pill_player::setup(project) {
            Ok(runtime) => launch(runtime),
            Err(error) => report_failure("project setup", &error),
        }
        return;
    };

    // Step 3: Otherwise download the pack, mount it, then initialize. The
    // fetch runs on the browser's event loop, so the page keeps painting.
    pill_core::platform::futures::spawn(async move {
        let bytes = match fetch_asset_pack(url).await {
            Ok(bytes) => bytes,
            Err(message) => {
                error!(target: telemetry_target::ENGINE, error = %message, "asset pack download failed");
                return;
            }
        };
        let database = match pill_player::mount_asset_pack(
            project.name,
            pill_player::PackSource::Fetched,
            bytes,
        ) {
            Ok(database) => database,
            Err(error) => return report_failure("asset pack mount", &error),
        };
        match pill_player::setup_with_asset_database(project, Some(database)) {
            Ok(runtime) => launch(runtime),
            Err(error) => report_failure("project setup", &error),
        }
    });
}

/// Download the asset pack at `url`, relative to the page.
///
/// The whole file arrives as one buffer: a pack is a single indexed blob, so
/// there is nothing to stream before it is parsed.
async fn fetch_asset_pack(url: &str) -> Result<Vec<u8>, String> {
    let window = web_sys::window().ok_or_else(|| "the page has no window".to_owned())?;
    let response = wasm_bindgen_futures::JsFuture::from(window.fetch_with_str(url))
        .await
        .map_err(|error| format!("{url}: {error:?}"))?;
    let response: web_sys::Response = response
        .dyn_into()
        .map_err(|_| format!("{url}: the fetch did not answer with a response"))?;
    if !response.ok() {
        return Err(format!("{url}: HTTP {}", response.status()));
    }
    let buffer = wasm_bindgen_futures::JsFuture::from(
        response
            .array_buffer()
            .map_err(|error| format!("{url}: {error:?}"))?,
    )
    .await
    .map_err(|error| format!("{url}: {error:?}"))?;
    Ok(js_sys::Uint8Array::new(&buffer).to_vec())
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
        if let Err(error) = focus_without_scrolling(&focus_target) {
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
            let attach = pill_player::attach_renderer(runtime, Arc::clone(&window), width, height);
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
                        attached.resize_surface(SurfaceId::PRIMARY, size.width, size.height);
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
        for input in translate_window_event(&event) {
            if let Some(driver) = self.driver.borrow_mut().as_mut() {
                driver.push_input(input);
            }
        }
        match event {
            WindowEvent::Resized(size) => {
                if let Some(driver) = self.driver.borrow_mut().as_mut() {
                    driver.resize_surface(SurfaceId::PRIMARY, size.width, size.height);
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
pub(crate) fn find_canvas(canvas_id: &str) -> Result<HtmlCanvasElement, String> {
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

/// Focus `element` without scrolling it into view.
///
/// A plain `focus()` scrolls the element into view, which cancels any scroll
/// in flight (for example a smooth scroll toward an anchor below the canvas)
/// whenever it runs mid scroll. `web-sys` has no typed `focus(options)`, so
/// the call goes through raw JS.
///
/// # Errors
///
/// Returns the JS error if `focus` is missing or the call rejects.
pub(crate) fn focus_without_scrolling(
    element: &HtmlCanvasElement,
) -> Result<(), wasm_bindgen::JsValue> {
    let element: &wasm_bindgen::JsValue = element.as_ref();
    let options = js_sys::Object::new();
    js_sys::Reflect::set(
        &options,
        &wasm_bindgen::JsValue::from_str("preventScroll"),
        &wasm_bindgen::JsValue::TRUE,
    )?;

    let focus = js_sys::Reflect::get(element, &wasm_bindgen::JsValue::from_str("focus"))?;
    let focus = focus
        .dyn_into::<js_sys::Function>()
        .map_err(|_| wasm_bindgen::JsValue::from_str("focus is not a function"))?;
    focus.call1(element, &options)?;

    Ok(())
}

/// The canvas's laid-out size in physical pixels, at least 1x1.
pub(crate) fn physical_size(canvas: &HtmlCanvasElement) -> (u32, u32) {
    let ratio = web_sys::window().map_or(1.0, |window| window.device_pixel_ratio());
    let scale = |css_pixels: i32| (f64::from(css_pixels) * ratio).round().max(1.0) as u32;
    (scale(canvas.client_width()), scale(canvas.client_height()))
}

/// Report a failure to the log, with its whole cause chain.
pub(crate) fn report_failure(context: &str, error: &(dyn std::error::Error + 'static)) {
    let cause_chain = format_error_chain(error);
    error!(target: telemetry_target::ENGINE, error = %cause_chain, "{} failed", context);
}
