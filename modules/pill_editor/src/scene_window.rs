//! A native X11 child window the engine presents into on Linux.
//!
//! # Responsibilities
//!
//! - Give the engine's swapchain a native window of its own, parented to an
//!   editor window and placed over the Scene panel.
//! - Leave pointer and keyboard input to the WebView underneath it.
//!
//! # Design
//!
//! On Windows the WebView is a child window and the OS composites it with the
//! engine's surface. On Linux dioxus-desktop builds the WebView as a GTK widget
//! inside the same native window the engine presents to, so the two painters
//! overwrite each other's pixels, and window transparency cannot arbitrate
//! between them. An X11 child window restores what Windows does, turned the
//! other way up: the X server clips the parent's (WebView's) drawing against
//! the child, the engine is the only painter of the child, and neither side
//! needs an alpha channel.
//!
//! The window is created on its own Xlib connection rather than GDK's, so GTK
//! never learns about it and never paints or destroys it. Closing that
//! connection destroys the window, which is also safe when the parent is
//! already gone, whereas an explicit `XDestroyWindow` on a dead window would
//! raise a fatal X error.
//!
//! Its input region is emptied with XFixes, so clicks and hover over the scene
//! reach the WebView as though the window were not there. The dock's Scene
//! panel keeps its HTML behaviour; only its pixels come from the engine.
//!
//! Wayland has no equivalent a GTK3 widget can host, so [`SceneChildWindow::new`]
//! returns `None` there and the editor falls back to presenting straight into
//! its own window. [`prefer_x11_backend`] steers the editor onto XWayland so
//! that fallback is the exception.

use std::ffi::c_void;
use std::ptr::NonNull;

use dioxus::desktop::tao::window::Window;
use pill_host::RenderViewport;
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle,
    RawWindowHandle, WindowHandle, XlibDisplayHandle, XlibWindowHandle,
};
use x11_dl::{xfixes, xlib};

/// The Shape extension's input region kind (`ShapeInput` in `X11/extensions/shape.h`).
const SHAPE_INPUT: i32 = 2;

/// Environment variable that keeps the editor on native Wayland.
pub(crate) const NATIVE_WAYLAND_ENV: &str = "PILL_EDITOR_NATIVE_WAYLAND";

/// Run the editor's GTK on XWayland unless the user opted out.
///
/// Native Wayland offers no child surface a GTK3 widget can own, so there the
/// engine and the WebView keep sharing one surface and flicker. XWayland is
/// part of every mainstream Wayland session, and under it the scene gets its
/// own X11 child window. Must run before GTK initialises, i.e. before the
/// Dioxus launch.
pub(crate) fn prefer_x11_backend() {
    if std::env::var_os(NATIVE_WAYLAND_ENV).is_some() || std::env::var_os("DISPLAY").is_none() {
        return;
    }
    if std::env::var("GDK_BACKEND").as_deref() != Ok("x11") {
        println!(
            "[editor] Using GDK_BACKEND=x11 so the scene gets its own native window; \
             set {NATIVE_WAYLAND_ENV}=1 to stay on Wayland"
        );
        std::env::set_var("GDK_BACKEND", "x11");
    }
}

/// An X11 child window owned by the renderer's surface.
///
/// Shared between the editor, which places it, and the renderer, which keeps
/// it alive for as long as its swapchain exists.
pub(crate) struct SceneChildWindow {
    xlib: xlib::Xlib,
    display: NonNull<xlib::Display>,
    screen: i32,
    window: xlib::Window,
}

// SAFETY: the display connection is private to this value and only used from
// the thread that runs the editor's event loop and renderer; `Send + Sync` is
// what wgpu requires of a window handle it keeps alive, not a sign of use
// across threads.
unsafe impl Send for SceneChildWindow {}
// SAFETY: see the `Send` implementation above.
unsafe impl Sync for SceneChildWindow {}

impl SceneChildWindow {
    /// Create an unmapped child of `parent`, or `None` when `parent` is not an
    /// X11 window or the X libraries cannot be loaded.
    pub(crate) fn new(parent: &Window) -> Option<Self> {
        let RawWindowHandle::Xlib(parent_handle) = parent.window_handle().ok()?.as_raw() else {
            return None;
        };
        let xlib = xlib::Xlib::open().ok()?;
        // SAFETY: `XOpenDisplay` with a null name opens `$DISPLAY`; a null
        // result is handled below.
        let display = NonNull::new(unsafe { (xlib.XOpenDisplay)(std::ptr::null()) })?;
        // SAFETY: `display` is a live connection for every call in this block,
        // and `parent_handle.window` is the XID tao reported for the parent.
        unsafe {
            let raw = display.as_ptr();
            let screen = (xlib.XDefaultScreen)(raw);
            let window = (xlib.XCreateSimpleWindow)(
                raw,
                parent_handle.window,
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

    /// Cover `viewport` (physical pixels of the parent), or hide the window
    /// when the viewport is empty.
    pub(crate) fn place(&self, viewport: RenderViewport) {
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
    }

    /// Restack the window above its siblings.
    ///
    /// WebKit may create or restack native windows of its own inside the same
    /// parent; this keeps the scene above them. It is a single asynchronous
    /// request, cheap enough to send every frame.
    pub(crate) fn raise(&self) {
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

impl Drop for SceneChildWindow {
    fn drop(&mut self) {
        // Closing the connection destroys the window it created (the default
        // close-down mode), with no error if the parent already took it down.
        // SAFETY: the connection is owned by `self` and not used afterwards.
        unsafe {
            (self.xlib.XCloseDisplay)(self.display.as_ptr());
        }
    }
}

impl HasWindowHandle for SceneChildWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let raw = RawWindowHandle::Xlib(XlibWindowHandle::new(self.window));
        // SAFETY: the window lives as long as `self`, which outlives the borrow.
        Ok(unsafe { WindowHandle::borrow_raw(raw) })
    }
}

impl HasDisplayHandle for SceneChildWindow {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        let display = NonNull::new(self.display.as_ptr().cast::<c_void>());
        let raw = RawDisplayHandle::Xlib(XlibDisplayHandle::new(display, self.screen));
        // SAFETY: the connection lives as long as `self`, which outlives the borrow.
        Ok(unsafe { DisplayHandle::borrow_raw(raw) })
    }
}

/// The size a renderer targeting a child window placed at `viewport` needs.
///
/// A hidden (empty) viewport reports zero, which the renderer treats as
/// minimised and skips rather than presenting to an unmapped window.
pub(crate) fn surface_size(viewport: RenderViewport) -> (u32, u32) {
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
}
