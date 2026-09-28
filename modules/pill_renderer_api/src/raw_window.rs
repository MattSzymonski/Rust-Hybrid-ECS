//! A window's platform identity as plain data, for handing a window to a
//! renderer that was built separately from the frontend that owns it.
//!
//! # Responsibilities
//!
//! - Define [`RawWindowData`], a `#[repr(C)]` value holding the handles a GPU
//!   surface is created from.
//! - Convert to it from any window that implements `raw-window-handle` 0.6
//!   (winit, tao), and back to the `raw-window-handle` values wgpu takes.
//!
//! # Design
//!
//! The frontend owns the window and its event loop; the renderer only needs to
//! create a surface on it. Passing a window *type* across that boundary would
//! tie the renderer to the frontend's windowing crate - two copies of winit in
//! one process, or a renderer that cannot take the editor's tao window. The
//! handles themselves are integers and pointers, so they cross as data: every
//! field is a fixed-width integer, which keeps the layout stable across the
//! artifacts that exchange it.
//!
//! The data does not keep the window alive. Whoever hands it out guarantees
//! the window outlives every surface created from it.

// External crates
pub use raw_window_handle;
use raw_window_handle::{
    AppKitDisplayHandle, AppKitWindowHandle, HasDisplayHandle, HasWindowHandle, RawDisplayHandle,
    RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle, Win32WindowHandle,
    WindowsDisplayHandle, XcbDisplayHandle, XcbWindowHandle, XlibDisplayHandle, XlibWindowHandle,
};

// Current crate
use crate::error::RendererError;

/// Which platform's handles a [`RawWindowData`] carries.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawWindowKind {
    /// Windows: `window` is the `HWND`, `window_aux` the `HINSTANCE` (or 0).
    Win32 = 1,
    /// X11 through Xlib: `window` is the window id, `window_aux` the visual id
    /// (or 0), `display` the `Display*` (or 0), `screen` the screen.
    Xlib = 2,
    /// X11 through XCB: `window` is the window id, `window_aux` the visual id
    /// (or 0), `display` the connection (or 0), `screen` the screen.
    Xcb = 3,
    /// Wayland: `window` is the `wl_surface*`, `display` the `wl_display*`.
    Wayland = 4,
    /// macOS: `window` is the `NSView*`.
    AppKit = 5,
}

/// A window's platform handles, as plain data.
///
/// Only the fields [`RawWindowKind`] names for `kind` are meaningful; the rest
/// are zero.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawWindowData {
    /// Which platform the handles belong to.
    pub kind: RawWindowKind,
    /// The window itself: a handle, an id or a pointer, by `kind`.
    pub window: u64,
    /// A second window-side value some platforms carry.
    pub window_aux: u64,
    /// The display connection, where the platform has one.
    pub display: u64,
    /// The X11 screen number; zero elsewhere.
    pub screen: i32,
}

impl RawWindowData {
    /// Read the handles of `window`.
    ///
    /// # Errors
    ///
    /// Returns [`RendererError::SurfaceCreation`] when the window cannot give
    /// its handles right now, or its platform is not one this type carries.
    pub fn from_window(
        window: &(impl HasWindowHandle + HasDisplayHandle + ?Sized),
    ) -> Result<Self, RendererError> {
        let window_handle = window.window_handle().map_err(unavailable)?.as_raw();
        let display_handle = window.display_handle().map_err(unavailable)?.as_raw();
        Self::from_raw(window_handle, display_handle)
    }

    /// Carry already-extracted handles.
    ///
    /// # Errors
    ///
    /// Returns [`RendererError::SurfaceCreation`] for a platform this type
    /// does not carry, or a window and display handle from different ones.
    pub fn from_raw(
        window: RawWindowHandle,
        display: RawDisplayHandle,
    ) -> Result<Self, RendererError> {
        let empty = |kind| Self {
            kind,
            window: 0,
            window_aux: 0,
            display: 0,
            screen: 0,
        };
        let pointer = |pointer: Option<std::ptr::NonNull<core::ffi::c_void>>| {
            pointer.map_or(0, |pointer| pointer.as_ptr() as usize as u64)
        };
        match (window, display) {
            (RawWindowHandle::Win32(handle), RawDisplayHandle::Windows(_)) => Ok(Self {
                window: handle.hwnd.get() as u64,
                window_aux: handle.hinstance.map_or(0, |instance| instance.get() as u64),
                ..empty(RawWindowKind::Win32)
            }),
            (RawWindowHandle::Xlib(handle), RawDisplayHandle::Xlib(display)) => Ok(Self {
                window: u64::from(handle.window),
                window_aux: u64::from(handle.visual_id),
                display: pointer(display.display),
                screen: display.screen,
                ..empty(RawWindowKind::Xlib)
            }),
            (RawWindowHandle::Xcb(handle), RawDisplayHandle::Xcb(display)) => Ok(Self {
                window: u64::from(handle.window.get()),
                window_aux: handle.visual_id.map_or(0, |visual| u64::from(visual.get())),
                display: pointer(display.connection),
                screen: display.screen,
                ..empty(RawWindowKind::Xcb)
            }),
            (RawWindowHandle::Wayland(handle), RawDisplayHandle::Wayland(display)) => Ok(Self {
                window: pointer(Some(handle.surface)),
                display: pointer(Some(display.display)),
                ..empty(RawWindowKind::Wayland)
            }),
            (RawWindowHandle::AppKit(handle), RawDisplayHandle::AppKit(_)) => Ok(Self {
                window: pointer(Some(handle.ns_view)),
                ..empty(RawWindowKind::AppKit)
            }),
            (window, display) => Err(RendererError::SurfaceCreation {
                detail: format!(
                    "window handles of this platform are not supported: {window:?} on {display:?}"
                ),
            }),
        }
    }

    /// The `raw-window-handle` values a GPU surface is created from.
    ///
    /// # Errors
    ///
    /// Returns [`RendererError::SurfaceCreation`] when a handle that must not
    /// be zero is zero.
    pub fn to_raw(&self) -> Result<(RawWindowHandle, RawDisplayHandle), RendererError> {
        let missing = |what: &str| RendererError::SurfaceCreation {
            detail: format!("the window data carries no {what}"),
        };
        let pointer = |value: u64| std::ptr::NonNull::new(value as usize as *mut core::ffi::c_void);
        match self.kind {
            RawWindowKind::Win32 => {
                let hwnd = std::num::NonZeroIsize::new(self.window as isize)
                    .ok_or_else(|| missing("window handle"))?;
                let mut window = Win32WindowHandle::new(hwnd);
                window.hinstance = std::num::NonZeroIsize::new(self.window_aux as isize);
                Ok((window.into(), WindowsDisplayHandle::new().into()))
            }
            RawWindowKind::Xlib => {
                let mut window = XlibWindowHandle::new(self.window as _);
                window.visual_id = self.window_aux as _;
                Ok((
                    window.into(),
                    XlibDisplayHandle::new(pointer(self.display), self.screen).into(),
                ))
            }
            RawWindowKind::Xcb => {
                let id = std::num::NonZeroU32::new(self.window as u32)
                    .ok_or_else(|| missing("window id"))?;
                let mut window = XcbWindowHandle::new(id);
                window.visual_id = std::num::NonZeroU32::new(self.window_aux as u32);
                Ok((
                    window.into(),
                    XcbDisplayHandle::new(pointer(self.display), self.screen).into(),
                ))
            }
            RawWindowKind::Wayland => {
                let surface = pointer(self.window).ok_or_else(|| missing("surface"))?;
                let display = pointer(self.display).ok_or_else(|| missing("display"))?;
                Ok((
                    WaylandWindowHandle::new(surface).into(),
                    WaylandDisplayHandle::new(display).into(),
                ))
            }
            RawWindowKind::AppKit => {
                let view = pointer(self.window).ok_or_else(|| missing("view"))?;
                Ok((
                    AppKitWindowHandle::new(view).into(),
                    AppKitDisplayHandle::new().into(),
                ))
            }
        }
    }
}

/// A window that could not hand out its handles, as a surface failure.
fn unavailable(error: raw_window_handle::HandleError) -> RendererError {
    RendererError::SurfaceCreation {
        detail: format!("the window's handles are unavailable: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Win32 handles survive the trip to plain data and back.
    #[test]
    fn win32_handles_round_trip() {
        let mut window = Win32WindowHandle::new(std::num::NonZeroIsize::new(0x1234).unwrap());
        window.hinstance = std::num::NonZeroIsize::new(0x5678);
        let data =
            RawWindowData::from_raw(window.into(), WindowsDisplayHandle::new().into()).unwrap();

        assert_eq!(data.kind, RawWindowKind::Win32);
        let (window_back, display_back) = data.to_raw().unwrap();
        assert_eq!(window_back, RawWindowHandle::Win32(window));
        assert_eq!(
            display_back,
            RawDisplayHandle::Windows(WindowsDisplayHandle::new())
        );
    }

    /// Xcb handles, including the screen, survive the trip.
    #[test]
    fn xcb_handles_round_trip() {
        let mut window = XcbWindowHandle::new(std::num::NonZeroU32::new(42).unwrap());
        window.visual_id = std::num::NonZeroU32::new(7);
        let display = XcbDisplayHandle::new(None, 2);
        let data = RawWindowData::from_raw(window.into(), display.into()).unwrap();

        let (window_back, display_back) = data.to_raw().unwrap();
        assert_eq!(window_back, RawWindowHandle::Xcb(window));
        assert_eq!(display_back, RawDisplayHandle::Xcb(display));
    }

    /// A zero window handle is refused rather than turned into a surface.
    #[test]
    fn a_zero_window_handle_is_refused() {
        let data = RawWindowData {
            kind: RawWindowKind::Win32,
            window: 0,
            window_aux: 0,
            display: 0,
            screen: 0,
        };

        assert!(data.to_raw().is_err());
    }

    /// Handles of different platforms are refused rather than mixed.
    #[test]
    fn mismatched_platforms_are_refused() {
        let window = Win32WindowHandle::new(std::num::NonZeroIsize::new(1).unwrap());

        assert!(RawWindowData::from_raw(window.into(), AppKitDisplayHandle::new().into()).is_err());
    }
}
