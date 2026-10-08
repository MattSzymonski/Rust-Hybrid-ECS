//! A native surface of its own for the engine on Linux.
//!
//! # Responsibilities
//!
//! - Give the engine's swapchain a native surface separate from the WebView's,
//!   attached to an editor window and placed over the Scene panel.
//! - Leave pointer and keyboard input to the WebView.
//!
//! # Design
//!
//! On Windows the WebView is a child window of its own, and the OS compositor
//! blends it, per pixel, over the engine's surface. On Linux dioxus-desktop
//! builds the WebView as a GTK widget inside the very native window the engine
//! would present to, so the two painters produce one buffer and overwrite each
//! other. The engine therefore gets a surface of its own.
//!
//! - **Wayland** ([`WaylandScene`]): a `wl_subsurface` of the GTK window's
//!   surface, stacked *below* it. The compositor blends GTK's surface over it
//!   with GTK's alpha, so wherever the WebView is transparent (the Scene
//!   panel) the scene shows through, and any HTML drawn there sits on top of
//!   it - the same arrangement as Windows. The subsurface is created on GDK's
//!   own Wayland connection, because Wayland objects cannot be related across
//!   connections, and is desynchronised so the engine presents at its own
//!   rate. Its position is parent state, applied on GTK's next commit.
//!
//!   Frames are paced on the subsurface's own frame callbacks
//!   ([`SceneChildWindow::begin_frame`]). The renderer presents with FIFO,
//!   and Mesa's Wayland FIFO blocks the next present until the compositor
//!   answers the previous frame's callback - which it never does while the
//!   window is covered or minimised, nor for a new subsurface before GTK's
//!   next commit places it. Blocking there would stall GTK's own main loop on
//!   the same thread, so the editor instead only presents once the previous
//!   frame has been answered.
//! - **X11** ([`X11Scene`]): X11 never blends a child window with its parent,
//!   so there the engine gets a child window stacked *above* the WebView. The
//!   scene is visible, but HTML over the Scene panel is hidden under it.
//!
//! In both cases the engine's surface has an empty input region, so the Scene
//! panel's HTML keeps receiving pointer and keyboard input.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Arc;

use dioxus::desktop::tao::platform::unix::WindowExtUnix;
use dioxus::desktop::tao::window::Window;
use gtk::prelude::WidgetExt;
use pill_host::RenderViewport;
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle,
    RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle, WindowHandle, XlibDisplayHandle,
    XlibWindowHandle,
};
use wayland_client::backend::{Backend, ObjectId};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{
    wl_callback::{self, WlCallback},
    wl_compositor::WlCompositor,
    wl_region::WlRegion,
    wl_registry::WlRegistry,
    wl_subcompositor::WlSubcompositor,
    wl_subsurface::WlSubsurface,
    wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use x11_dl::{xfixes, xlib};

/// The Shape extension's input region kind (`ShapeInput` in `X11/extensions/shape.h`).
const SHAPE_INPUT: i32 = 2;

/// Whether GTK is going to run on Wayland in this process.
///
/// Decided from the environment because it is needed before the first window
/// exists: a window that shows the scene through its WebView must be created
/// transparent. Mirrors GDK's own choice - the first entry of `GDK_BACKEND`,
/// or Wayland first when it is unset.
pub(crate) fn session_is_wayland() -> bool {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return false;
    }
    match std::env::var("GDK_BACKEND") {
        Ok(backends) => matches!(
            backends.split(',').next().map(str::trim),
            Some("wayland" | "*" | "")
        ),
        Err(_) => true,
    }
}

/// The engine's own native surface for one editor window.
///
/// Shared between the editor, which places it, and the renderer, which keeps
/// it alive for as long as its swapchain exists.
pub(crate) struct SceneChildWindow {
    /// The editor window the surface belongs to.
    parent: Arc<Window>,
    kind: Kind,
    /// Whether the last [`Self::place`] put the surface on screen.
    shown: Cell<bool>,
}

/// Boxed: the Xlib function table alone is several kilobytes, and the
/// Wayland state is a few hundred bytes.
enum Kind {
    Wayland(Box<WaylandScene>),
    X11(Box<X11Scene>),
}

// SAFETY: every Xlib and Wayland call, and every `Cell`/`RefCell` access, is
// made from the thread that runs the editor's event loop and renderer. wgpu
// requires `Send + Sync` of a window handle it keeps alive; it does not use
// this value from other threads.
unsafe impl Send for SceneChildWindow {}
// SAFETY: see the `Send` implementation above.
unsafe impl Sync for SceneChildWindow {}

impl SceneChildWindow {
    /// Create the engine's surface for `parent`, hidden until [`Self::place`].
    ///
    /// Returns `None` when the parent is neither a Wayland nor an X11 window,
    /// or when the platform libraries cannot be loaded; the editor then falls
    /// back to presenting into the parent itself.
    pub(crate) fn new(parent: &Arc<Window>) -> Option<Self> {
        let kind = match parent.window_handle().ok()?.as_raw() {
            RawWindowHandle::Wayland(_) => Kind::Wayland(Box::new(WaylandScene::new(parent)?)),
            RawWindowHandle::Xlib(handle) => Kind::X11(Box::new(X11Scene::new(handle.window)?)),
            _ => return None,
        };
        Some(Self {
            parent: Arc::clone(parent),
            kind,
            shown: Cell::new(false),
        })
    }

    /// The parent's WebView size in physical pixels: what a surface filling
    /// the whole window has to cover.
    pub(crate) fn webview_size(&self) -> (u32, u32) {
        let scale = self.parent.scale_factor();
        match webview_logical_size(&self.parent) {
            Some((width, height)) => (
                (width * scale).round() as u32,
                (height * scale).round() as u32,
            ),
            None => self.parent.inner_size().into(),
        }
    }

    /// Cover `viewport` (physical pixels of the parent's WebView), or hide the
    /// surface when it is empty.
    ///
    /// Returns the size the renderer has to present at. A hidden surface
    /// reports zero, which the renderer treats as minimised and skips rather
    /// than presenting to a surface nobody sees.
    pub(crate) fn place(&self, viewport: RenderViewport) -> (u32, u32) {
        let size = match &self.kind {
            Kind::Wayland(scene) => scene.place(&self.parent, viewport),
            Kind::X11(scene) => scene.place(viewport),
        };
        self.shown.set(size != (0, 0));
        size
    }

    /// Whether the surface is on screen, i.e. the renderer presents to it.
    pub(crate) fn is_shown(&self) -> bool {
        self.shown.get()
    }

    /// Per-frame upkeep: follow the parent when GTK moves or replaces what the
    /// surface is attached to.
    pub(crate) fn refresh(&self) {
        match &self.kind {
            Kind::Wayland(scene) => scene.refresh(&self.parent),
            Kind::X11(scene) => scene.raise(),
        }
    }

    /// Whether the engine may run a frame now, which may present.
    ///
    /// `false` while the compositor has not yet shown the previous frame;
    /// presenting then would block the calling thread inside the driver. Call
    /// [`Self::end_frame`] after every frame this allowed.
    pub(crate) fn begin_frame(&self) -> bool {
        match &self.kind {
            Kind::Wayland(scene) => scene.begin_frame(),
            Kind::X11(_) => true,
        }
    }

    /// Report whether the frame [`Self::begin_frame`] allowed presented.
    pub(crate) fn end_frame(&self, presented: bool) {
        if let Kind::Wayland(scene) = &self.kind {
            scene.end_frame(presented);
        }
    }
}

impl HasWindowHandle for SceneChildWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let raw = match &self.kind {
            Kind::Wayland(scene) => RawWindowHandle::Wayland(WaylandWindowHandle::new(
                NonNull::new(scene.surface.id().as_ptr().cast::<c_void>())
                    .ok_or(HandleError::Unavailable)?,
            )),
            Kind::X11(scene) => RawWindowHandle::Xlib(XlibWindowHandle::new(scene.window)),
        };
        // SAFETY: the surface or window lives as long as `self`, which
        // outlives the borrow.
        Ok(unsafe { WindowHandle::borrow_raw(raw) })
    }
}

impl HasDisplayHandle for SceneChildWindow {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        let raw = match &self.kind {
            Kind::Wayland(scene) => RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
                NonNull::new(scene.connection.backend().display_ptr().cast::<c_void>())
                    .ok_or(HandleError::Unavailable)?,
            )),
            Kind::X11(scene) => RawDisplayHandle::Xlib(XlibDisplayHandle::new(
                NonNull::new(scene.display.as_ptr().cast::<c_void>()),
                scene.screen,
            )),
        };
        // SAFETY: the connection lives as long as `self`, which outlives the borrow.
        Ok(unsafe { DisplayHandle::borrow_raw(raw) })
    }
}

// =============================================================================
// Wayland
// =============================================================================

/// A desynchronised `wl_subsurface` stacked below a GTK window's surface.
///
/// Methods taking `parent` are handed the editor window it was created for;
/// that window's GTK surface is the subsurface's parent.
struct WaylandScene {
    /// GDK's own connection, borrowed: dropping it does not disconnect.
    connection: Connection,
    /// Private queue for the objects created here, so GTK never sees their events.
    queue: RefCell<EventQueue<SceneEvents>>,
    subcompositor: WlSubcompositor,
    /// The surface the engine presents into.
    surface: WlSurface,
    /// Buffer scale last set on `surface`.
    scale: Cell<u32>,
    /// Where the surface should be, in physical pixels of the WebView.
    viewport: Cell<RenderViewport>,
    /// The subsurface role while the surface is shown.
    attachment: RefCell<Option<Attachment>>,
    /// Where the latest frame callback stands.
    pacing: Cell<Pacing>,
}

/// Progress of the frame callback the engine's frames are paced on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pacing {
    /// No callback outstanding: the next frame may present.
    Idle,
    /// A callback is requested but not yet committed, because the frame it was
    /// requested for did not present; it rides along with the next present.
    Requested,
    /// A presented frame's callback has not been answered yet.
    InFlight,
}

impl Pacing {
    /// The state after a frame callback was answered.
    fn answered(self) -> Self {
        match self {
            Self::InFlight => Self::Idle,
            other => other,
        }
    }

    /// The state after the frame that `begin_frame` allowed ended.
    fn ended(self, presented: bool) -> Self {
        match self {
            Self::Requested if presented => Self::InFlight,
            other => other,
        }
    }
}

/// The subsurface role binding the engine's surface to one GTK surface.
struct Attachment {
    /// GTK's `wl_surface` at the time of attaching, compared every frame
    /// because GDK may replace it (for example across a hide and show).
    parent_surface: *mut c_void,
    subsurface: WlSubsurface,
    /// Position last sent, in the parent surface's logical coordinates.
    position: Option<(i32, i32)>,
}

impl WaylandScene {
    fn new(parent: &Arc<Window>) -> Option<Self> {
        let RawDisplayHandle::Wayland(display) = parent.display_handle().ok()?.as_raw() else {
            return None;
        };
        // SAFETY: tao reports GDK's live `wl_display`, which outlives every
        // editor window and therefore this value.
        let backend = unsafe { Backend::from_foreign_display(display.display.as_ptr().cast()) };
        let connection = Connection::from_backend(backend);
        let (globals, queue) = registry_queue_init::<SceneEvents>(&connection).ok()?;
        let handle = queue.handle();
        // Version 3 is the first with `set_buffer_scale`.
        let compositor: WlCompositor = globals.bind(&handle, 3..=4, ()).ok()?;
        let subcompositor: WlSubcompositor = globals.bind(&handle, 1..=1, ()).ok()?;

        let surface = compositor.create_surface(&handle, ());
        // An empty input region: the pointer passes through to GTK, above.
        let region = compositor.create_region(&handle, ());
        surface.set_input_region(Some(&region));
        region.destroy();
        let scale = buffer_scale(parent.scale_factor());
        surface.set_buffer_scale(scale as i32);
        surface.commit();
        connection.flush().ok()?;

        Some(Self {
            connection,
            queue: RefCell::new(queue),
            subcompositor,
            surface,
            scale: Cell::new(scale),
            viewport: Cell::new(RenderViewport::default()),
            attachment: RefCell::new(None),
            pacing: Cell::new(Pacing::Idle),
        })
    }

    fn place(&self, parent: &Window, viewport: RenderViewport) -> (u32, u32) {
        let scale = buffer_scale(parent.scale_factor());
        if scale != self.scale.replace(scale) {
            // Double-buffered: applied with the renderer's next frame, which
            // is already sized for it by the value returned below.
            self.surface.set_buffer_scale(scale as i32);
        }
        let viewport = snap_to_scale(viewport, scale);
        self.viewport.set(viewport);
        self.refresh(parent);
        surface_size(viewport)
    }

    fn refresh(&self, parent: &Window) {
        self.dispatch_events();

        let viewport = self.viewport.get();
        let visible = viewport.width > 0 && viewport.height > 0;
        let gtk_surface = if visible {
            parent_surface(parent)
        } else {
            None
        };

        let mut attachment = self.attachment.borrow_mut();
        let mut parent_needs_commit = false;
        let mut changed = false;

        // Destroying the role unmaps the surface at once, so hiding needs no
        // commit from GTK.
        if attachment
            .as_ref()
            .is_some_and(|current| Some(current.parent_surface) != gtk_surface)
        {
            if let Some(stale) = attachment.take() {
                stale.subsurface.destroy();
                // An unmapped surface's callback may never be answered.
                self.pacing.set(Pacing::Idle);
                changed = true;
            }
        }

        if attachment.is_none() {
            if let Some(gtk_surface) = gtk_surface {
                *attachment = self.attach(gtk_surface);
                parent_needs_commit = attachment.is_some();
                changed = true;
            }
        }

        if let Some(current) = attachment.as_mut() {
            let (offset_x, offset_y) = webview_offset(parent);
            let scale = self.scale.get();
            let position = (
                offset_x + (viewport.x / scale) as i32,
                offset_y + (viewport.y / scale) as i32,
            );
            if current.position != Some(position) {
                current.subsurface.set_position(position.0, position.1);
                current.position = Some(position);
                parent_needs_commit = true;
                changed = true;
            }
        }

        if changed {
            let _ = self.connection.flush();
        }
        if parent_needs_commit {
            // Subsurface position and stacking are parent state; a redraw
            // makes GTK commit its surface and apply them.
            parent.gtk_window().queue_draw();
        }
    }

    fn begin_frame(&self) -> bool {
        self.dispatch_events();
        match self.pacing.get() {
            Pacing::InFlight => false,
            Pacing::Requested => true,
            // A hidden surface is not presented to (the renderer is parked at
            // zero size), so there is nothing to pace.
            Pacing::Idle if self.attachment.borrow().is_none() => true,
            Pacing::Idle => {
                // Requested before the frame, so the present's commit carries
                // it: the answer means the compositor has shown that frame.
                let handle = self.queue.borrow().handle();
                self.surface.frame(&handle, ());
                self.pacing.set(Pacing::Requested);
                true
            }
        }
    }

    fn end_frame(&self, presented: bool) {
        self.pacing.set(self.pacing.get().ended(presented));
    }

    /// Dispatch this queue's events, which GTK's main loop has already read.
    fn dispatch_events(&self) {
        let mut events = SceneEvents::default();
        let _ = self.queue.borrow_mut().dispatch_pending(&mut events);
        if events.frame_answered {
            self.pacing.set(self.pacing.get().answered());
        }
    }

    /// Give the engine's surface the subsurface role under `parent_surface`.
    fn attach(&self, parent_surface: *mut c_void) -> Option<Attachment> {
        // SAFETY: `parent_surface` is the live `wl_surface` GDK reported for
        // the parent window on this connection this frame.
        let id =
            unsafe { ObjectId::from_ptr(WlSurface::interface(), parent_surface.cast()) }.ok()?;
        let parent = WlSurface::from_id(&self.connection, id).ok()?;
        let handle = self.queue.borrow().handle();
        let subsurface = self
            .subcompositor
            .get_subsurface(&self.surface, &parent, &handle, ());
        subsurface.place_below(&parent);
        subsurface.set_desync();
        Some(Attachment {
            parent_surface,
            subsurface,
            position: None,
        })
    }
}

/// GTK's current `wl_surface` for `window`, if it has one.
fn parent_surface(window: &Window) -> Option<*mut c_void> {
    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Wayland(handle) => {
            Some(handle.surface.as_ptr()).filter(|surface| !surface.is_null())
        }
        _ => None,
    }
}

/// Where the WebView starts inside GTK's surface, in logical pixels.
///
/// GTK draws client-side decorations - shadow and title bar - inside its own
/// surface on Wayland, so the WebView's origin is not the surface's. The
/// WebView fills tao's box, which has no GDK window of its own, so the box's
/// allocation is already in the surface's coordinates.
fn webview_offset(window: &Window) -> (i32, i32) {
    window
        .default_vbox()
        .map(|vbox| {
            let allocation = vbox.allocation();
            (allocation.x().max(0), allocation.y().max(0))
        })
        .unwrap_or((0, 0))
}

/// The WebView's size in logical (CSS) pixels, once GTK has allocated it.
///
/// tao's `inner_size` is GTK's configure size, which on Wayland includes the
/// client-side shadow and title bar; the WebView fills tao's box, so the box's
/// allocation is the size the HTML lays out in. Under X11 the two agree.
/// `None` before the first allocation.
pub(crate) fn webview_logical_size(window: &Window) -> Option<(f64, f64)> {
    let allocation = window.default_vbox()?.allocation();
    (allocation.width() > 1 && allocation.height() > 1).then(|| {
        (
            f64::from(allocation.width()),
            f64::from(allocation.height()),
        )
    })
}

impl Drop for WaylandScene {
    fn drop(&mut self) {
        // wgpu has already destroyed its swapchain: it drops the surface
        // before the handle source that owns this value.
        if let Some(attachment) = self.attachment.get_mut().take() {
            attachment.subsurface.destroy();
        }
        self.surface.destroy();
        self.subcompositor.destroy();
        let _ = self.connection.flush();
    }
}

/// What one dispatch of the queue observed; only frame callbacks matter.
#[derive(Default)]
struct SceneEvents {
    frame_answered: bool,
}

impl Dispatch<WlCallback, ()> for SceneEvents {
    fn event(
        state: &mut Self,
        _: &WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.frame_answered = true;
        }
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for SceneEvents {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

wayland_client::delegate_noop!(SceneEvents: WlCompositor);
wayland_client::delegate_noop!(SceneEvents: WlSubcompositor);
wayland_client::delegate_noop!(SceneEvents: WlRegion);
wayland_client::delegate_noop!(SceneEvents: WlSubsurface);
wayland_client::delegate_noop!(SceneEvents: ignore WlSurface);

/// GTK3 scales by whole numbers; so does a `wl_surface` buffer.
fn buffer_scale(scale_factor: f64) -> u32 {
    scale_factor.round().max(1.0) as u32
}

/// Shrink a physical viewport to whole logical pixels.
///
/// A buffer whose size is not a multiple of its surface's buffer scale is a
/// protocol error, which would take down GTK's connection with it.
fn snap_to_scale(viewport: RenderViewport, scale: u32) -> RenderViewport {
    RenderViewport::new(
        viewport.x,
        viewport.y,
        viewport.width - viewport.width % scale,
        viewport.height - viewport.height % scale,
    )
}

// =============================================================================
// X11
// =============================================================================

/// An X11 child window stacked above the WebView.
///
/// Created on its own Xlib connection rather than GDK's, so GTK never learns
/// about it and never paints or destroys it. Closing that connection destroys
/// the window, which is also safe when the parent is already gone, whereas an
/// explicit `XDestroyWindow` on a dead window would raise a fatal X error.
struct X11Scene {
    xlib: xlib::Xlib,
    display: NonNull<xlib::Display>,
    screen: i32,
    window: xlib::Window,
}

impl X11Scene {
    /// Create an unmapped child of the X11 window `parent`.
    fn new(parent: xlib::Window) -> Option<Self> {
        let xlib = xlib::Xlib::open().ok()?;
        // SAFETY: `XOpenDisplay` with a null name opens `$DISPLAY`; a null
        // result is handled below.
        let display = NonNull::new(unsafe { (xlib.XOpenDisplay)(std::ptr::null()) })?;
        // SAFETY: `display` is a live connection for every call in this block,
        // and `parent` is the XID tao reported for the parent.
        unsafe {
            let raw = display.as_ptr();
            let screen = (xlib.XDefaultScreen)(raw);
            let window = (xlib.XCreateSimpleWindow)(
                raw,
                parent,
                0,
                0,
                1,
                1,
                0,
                0,
                (xlib.XBlackPixel)(raw, screen),
            );
            // No background: the server must not clear the window to black
            // on a resize or expose before the engine's next frame lands.
            (xlib.XSetWindowBackgroundPixmap)(raw, window, 0);
            make_input_transparent(raw, window);
            (xlib.XFlush)(raw);
            Some(Self {
                xlib,
                display,
                screen,
                window,
            })
        }
    }

    fn place(&self, viewport: RenderViewport) -> (u32, u32) {
        let raw = self.display.as_ptr();
        // SAFETY: `display` and `window` stay valid for the life of `self`.
        unsafe {
            if viewport.width == 0 || viewport.height == 0 {
                (self.xlib.XUnmapWindow)(raw, self.window);
            } else {
                (self.xlib.XMoveResizeWindow)(
                    raw,
                    self.window,
                    viewport.x as i32,
                    viewport.y as i32,
                    viewport.width,
                    viewport.height,
                );
                (self.xlib.XMapRaised)(raw, self.window);
            }
            (self.xlib.XFlush)(raw);
        }
        surface_size(viewport)
    }

    /// Restack the window above its siblings.
    ///
    /// WebKit may create or restack native windows of its own inside the same
    /// parent; this keeps the scene above them. It is a single asynchronous
    /// request, cheap enough to send every frame.
    fn raise(&self) {
        let raw = self.display.as_ptr();
        // SAFETY: `display` and `window` stay valid for the life of `self`.
        unsafe {
            (self.xlib.XRaiseWindow)(raw, self.window);
            (self.xlib.XFlush)(raw);
        }
    }
}

/// Empty the window's input region so events fall through to its parent.
///
/// Best effort: without XFixes the window still renders, it only swallows the
/// pointer over the scene.
///
/// # Safety
///
/// `display` must be a live connection and `window` a window created on it.
unsafe fn make_input_transparent(display: *mut xlib::Display, window: xlib::Window) {
    let Ok(xfixes) = xfixes::Xlib::open() else {
        return;
    };
    let (mut event_base, mut error_base) = (0, 0);
    // SAFETY: guaranteed by the caller.
    unsafe {
        if (xfixes.XFixesQueryExtension)(display, &mut event_base, &mut error_base) == 0 {
            return;
        }
        let region = (xfixes.XFixesCreateRegion)(display, std::ptr::null_mut(), 0);
        (xfixes.XFixesSetWindowShapeRegion)(display, window, SHAPE_INPUT, 0, 0, region);
        (xfixes.XFixesDestroyRegion)(display, region);
    }
}

impl Drop for X11Scene {
    fn drop(&mut self) {
        // Closing the connection destroys the window it created (the default
        // close-down mode), with no error if the parent already took it down.
        // SAFETY: the connection is owned by `self` and not used afterwards.
        unsafe {
            (self.xlib.XCloseDisplay)(self.display.as_ptr());
        }
    }
}

/// The size a renderer targeting a surface placed at `viewport` needs.
///
/// A hidden (empty) viewport reports zero, which the renderer treats as
/// minimised and skips rather than presenting to a surface nobody sees.
fn surface_size(viewport: RenderViewport) -> (u32, u32) {
    if viewport.width == 0 || viewport.height == 0 {
        (0, 0)
    } else {
        (viewport.width, viewport.height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A visible viewport sizes the surface to itself; an empty one parks it.
    #[test]
    fn surface_size_follows_the_viewport_and_parks_when_hidden() {
        assert_eq!(
            surface_size(RenderViewport::new(220, 48, 800, 600)),
            (800, 600)
        );
        assert_eq!(surface_size(RenderViewport::new(220, 48, 0, 600)), (0, 0));
        assert_eq!(surface_size(RenderViewport::default()), (0, 0));
    }

    /// A frame is only in flight once it presented, and only an answer ends it.
    #[test]
    fn pacing_waits_for_presented_frames_only() {
        // Presented: wait for the answer.
        assert_eq!(Pacing::Requested.ended(true), Pacing::InFlight);
        assert_eq!(Pacing::InFlight.answered(), Pacing::Idle);
        // Skipped: the request rides along with the next present instead.
        assert_eq!(Pacing::Requested.ended(false), Pacing::Requested);
        // An answer for nothing in flight changes nothing.
        assert_eq!(Pacing::Requested.answered(), Pacing::Requested);
        assert_eq!(Pacing::Idle.answered(), Pacing::Idle);
        assert_eq!(Pacing::Idle.ended(true), Pacing::Idle);
    }

    /// Buffer sizes stay whole multiples of the buffer scale.
    #[test]
    fn viewports_snap_to_the_buffer_scale() {
        assert_eq!(
            snap_to_scale(RenderViewport::new(441, 97, 1601, 1441), 2),
            RenderViewport::new(441, 97, 1600, 1440)
        );
        assert_eq!(
            snap_to_scale(RenderViewport::new(3, 5, 801, 601), 1),
            RenderViewport::new(3, 5, 801, 601)
        );
        assert_eq!(buffer_scale(1.0), 1);
        assert_eq!(buffer_scale(2.0), 2);
        assert_eq!(buffer_scale(0.0), 1);
    }
}
