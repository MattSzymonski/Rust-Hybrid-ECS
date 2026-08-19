//! Plain data types shared by the host and the runtime.
//!
//! # Responsibilities
//!
//! - Own [`FrameReport`], [`RenderViewport`], and [`VirtualResolution`], the
//!   three value types that cross the boundary every frame.
//! - Own [`PillWindowHandleV1`], the platform-neutral description of a native
//!   window the runtime binds its GPU surface to.
//!
//! # Design
//!
//! These types used to live in the engine and the host. They moved here
//! because a boundary type may not transitively reference engine, host, or
//! windowing types. Both sides re-export them, so existing call sites keep
//! their original import paths.
//!
//! [`PillWindowHandleV1`] deliberately carries *raw handles* rather than an
//! owned window pointer. The host owns its window for the whole process
//! lifetime and outlives every runtime generation, so nothing is transferred
//! and no reference count can leak or be released twice across a reload. It
//! also lets one contract describe both the `winit` window of the standalone
//! runner and the `tao` windows of the editor, including detached scene
//! windows.

// =============================================================================
// Constants
// =============================================================================

/// No native window is attached; the runtime runs headless.
pub const PILL_WINDOW_BACKEND_NONE: u32 = 0;

/// A Win32 window: `window_primary` is the `HWND`, `window_secondary` the
/// `HINSTANCE`.
pub const PILL_WINDOW_BACKEND_WIN32: u32 = 1;

/// An AppKit window: `window_primary` is the `NSView` pointer.
pub const PILL_WINDOW_BACKEND_APPKIT: u32 = 2;

/// An Xlib window: `window_primary` is the `Window` id, `window_secondary` the
/// visual id, `display_primary` the `Display` pointer, and
/// `display_secondary` the screen number.
pub const PILL_WINDOW_BACKEND_XLIB: u32 = 3;

/// An XCB window: `window_primary` is the window id, `window_secondary` the
/// visual id, `display_primary` the connection pointer, and
/// `display_secondary` the screen number.
pub const PILL_WINDOW_BACKEND_XCB: u32 = 4;

/// A Wayland window: `window_primary` is the `wl_surface` pointer and
/// `display_primary` the `wl_display` pointer.
pub const PILL_WINDOW_BACKEND_WAYLAND: u32 = 5;

// =============================================================================
// FrameReport
// =============================================================================

/// Frame statistics published by the runtime for host consoles and overlays.
///
/// `entity_count` is a fixed-width `u64` rather than the engine's `usize` so
/// the layout cannot depend on the pointer width of either side.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FrameReport {
    /// Frames per second measured over the reporting window.
    pub fps: f64,
    /// Number of live entities at report time.
    pub entity_count: u64,
}

// =============================================================================
// RenderViewport
// =============================================================================

/// Physical-pixel rectangle within a render target.
///
/// Sprite positions are interpreted relative to this rectangle's top-left
/// corner. The GPU viewport maps their local coordinates into the rectangle,
/// while a matching scissor prevents drawing outside it.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RenderViewport {
    /// Left edge of the rectangle, in physical pixels.
    pub x: u32,
    /// Top edge of the rectangle, in physical pixels.
    pub y: u32,
    /// Horizontal extent of the rectangle, in physical pixels.
    pub width: u32,
    /// Vertical extent of the rectangle, in physical pixels.
    pub height: u32,
}

impl RenderViewport {
    /// Construct a physical-pixel viewport rectangle.
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Construct a viewport covering an entire render target.
    pub const fn full(width: u32, height: u32) -> Self {
        Self::new(0, 0, width, height)
    }

    /// Clamp this rectangle to a render target, returning `None` when empty.
    pub fn clamped_to(self, target_width: u32, target_height: u32) -> Option<Self> {
        let x = self.x.min(target_width);
        let y = self.y.min(target_height);
        let width = self.width.min(target_width.saturating_sub(x));
        let height = self.height.min(target_height.saturating_sub(y));

        (width > 0 && height > 0).then_some(Self::new(x, y, width, height))
    }
}

// =============================================================================
// VirtualResolution
// =============================================================================

/// Logical coordinate space mapped into a physical [`RenderViewport`].
///
/// Keeping this separate from the swapchain dimensions lets an embedded project
/// keep a stable coordinate system while its dock panel is resized. The GPU
/// viewport performs the final scaling into the panel rectangle.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VirtualResolution {
    /// Horizontal extent of the project coordinate space.
    pub width: f32,
    /// Vertical extent of the project coordinate space.
    pub height: f32,
}

impl VirtualResolution {
    /// Construct a logical scene resolution.
    pub const fn new(width: f32, height: f32) -> Self {
        Self { width, height }
    }

    /// Return whether both dimensions can safely be used by the projection.
    pub fn is_valid(self) -> bool {
        self.width.is_finite() && self.height.is_finite() && self.width > 0.0 && self.height > 0.0
    }
}

// =============================================================================
// PillWindowHandleV1
// =============================================================================

/// Platform-neutral description of the native window the runtime renders into.
///
/// The struct is a flattened, fixed-width form of the platform window and
/// display handles. Every field is a `u64` so the layout is identical on 32-
/// and 64-bit targets, and unused slots for a given backend are zero.
///
/// # Safety
///
/// The handles are borrowed, not owned. The host must keep the described
/// window alive for as long as any runtime holds a surface created from it,
/// which the reload transaction guarantees by destroying the runtime before
/// the window and by never reloading while a frame call is in flight.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PillWindowHandleV1 {
    /// Size of this struct, used as the layout guard.
    pub struct_size: u32,
    /// Which `PILL_WINDOW_BACKEND_*` constant describes the payload.
    pub backend: u32,
    /// Primary window handle: `HWND`, `NSView*`, X11 window id, or `wl_surface*`.
    pub window_primary: u64,
    /// Secondary window handle: `HINSTANCE` or X11 visual id; zero otherwise.
    pub window_secondary: u64,
    /// Primary display handle: `Display*`, `xcb_connection_t*`, or `wl_display*`.
    pub display_primary: u64,
    /// Secondary display handle: the X11 screen number; zero otherwise.
    pub display_secondary: u64,
}

impl PillWindowHandleV1 {
    /// Build the descriptor of a headless run, where no surface is created.
    pub const fn none() -> Self {
        Self {
            struct_size: std::mem::size_of::<Self>() as u32,
            backend: PILL_WINDOW_BACKEND_NONE,
            window_primary: 0,
            window_secondary: 0,
            display_primary: 0,
            display_secondary: 0,
        }
    }

    /// Whether this descriptor was produced by a matching contract build.
    pub fn has_expected_layout(&self) -> bool {
        self.struct_size as usize == std::mem::size_of::<Self>()
    }

    /// Whether this descriptor names a real window rather than headless mode.
    pub fn describes_window(&self) -> bool {
        self.backend != PILL_WINDOW_BACKEND_NONE
    }
}

// =============================================================================
// PillWindowHandleV1 - raw-window-handle translation
// =============================================================================

#[cfg(feature = "window-handle")]
mod window_handle_translation {
    use super::*;
    use std::ffi::c_void;

    use raw_window_handle::{
        AppKitDisplayHandle, AppKitWindowHandle, RawDisplayHandle, RawWindowHandle,
        WaylandDisplayHandle, WaylandWindowHandle, Win32WindowHandle, WindowsDisplayHandle,
        XcbDisplayHandle, XcbWindowHandle, XlibDisplayHandle, XlibWindowHandle,
    };
    use std::num::{NonZeroIsize, NonZeroU32};
    use std::ptr::NonNull;

    impl PillWindowHandleV1 {
        /// Flatten a platform window and display handle pair into the contract
        /// form.
        ///
        /// Returns `None` for a platform this contract revision cannot
        /// describe, so an unsupported frontend fails loudly at surface setup
        /// instead of silently rendering nowhere.
        pub fn from_raw_handles(
            window_handle: RawWindowHandle,
            display_handle: RawDisplayHandle,
        ) -> Option<Self> {
            let mut descriptor = Self::none();

            // Step 1: Flatten the window half, which also selects the backend.
            match window_handle {
                RawWindowHandle::Win32(window) => {
                    descriptor.backend = PILL_WINDOW_BACKEND_WIN32;
                    descriptor.window_primary = window.hwnd.get() as u64;
                    descriptor.window_secondary = window
                        .hinstance
                        .map(|value| value.get() as u64)
                        .unwrap_or(0);
                }
                RawWindowHandle::AppKit(window) => {
                    descriptor.backend = PILL_WINDOW_BACKEND_APPKIT;
                    descriptor.window_primary = window.ns_view.as_ptr() as u64;
                }
                RawWindowHandle::Xlib(window) => {
                    descriptor.backend = PILL_WINDOW_BACKEND_XLIB;
                    descriptor.window_primary = window.window as u64;
                    descriptor.window_secondary = window.visual_id as u64;
                }
                RawWindowHandle::Xcb(window) => {
                    descriptor.backend = PILL_WINDOW_BACKEND_XCB;
                    descriptor.window_primary = window.window.get() as u64;
                    descriptor.window_secondary = window
                        .visual_id
                        .map(|value| value.get() as u64)
                        .unwrap_or(0);
                }
                RawWindowHandle::Wayland(window) => {
                    descriptor.backend = PILL_WINDOW_BACKEND_WAYLAND;
                    descriptor.window_primary = window.surface.as_ptr() as u64;
                }
                _ => return None,
            }

            // Step 2: Flatten the display half, which must agree with the
            // backend the window half selected.
            match display_handle {
                RawDisplayHandle::Windows(_) => {}
                RawDisplayHandle::AppKit(_) => {}
                RawDisplayHandle::Xlib(display) => {
                    descriptor.display_primary = display
                        .display
                        .map(|pointer| pointer.as_ptr() as u64)
                        .unwrap_or(0);
                    descriptor.display_secondary = display.screen as u64;
                }
                RawDisplayHandle::Xcb(display) => {
                    descriptor.display_primary = display
                        .connection
                        .map(|pointer| pointer.as_ptr() as u64)
                        .unwrap_or(0);
                    descriptor.display_secondary = display.screen as u64;
                }
                RawDisplayHandle::Wayland(display) => {
                    descriptor.display_primary = display.display.as_ptr() as u64;
                }
                _ => return None,
            }

            Some(descriptor)
        }

        /// Rebuild the platform window and display handles from the contract
        /// form.
        ///
        /// Returns `None` when the descriptor is headless, was produced by a
        /// different contract layout, or names a backend this build cannot
        /// reconstruct - for example a null Wayland surface pointer.
        pub fn to_raw_handles(self) -> Option<(RawWindowHandle, RawDisplayHandle)> {
            if !self.has_expected_layout() || !self.describes_window() {
                return None;
            }

            match self.backend {
                PILL_WINDOW_BACKEND_WIN32 => {
                    let hwnd = NonZeroIsize::new(self.window_primary as isize)?;
                    let mut window = Win32WindowHandle::new(hwnd);
                    window.hinstance = NonZeroIsize::new(self.window_secondary as isize);
                    Some((
                        RawWindowHandle::Win32(window),
                        RawDisplayHandle::Windows(WindowsDisplayHandle::new()),
                    ))
                }
                PILL_WINDOW_BACKEND_APPKIT => {
                    let ns_view = NonNull::new(self.window_primary as *mut c_void)?;
                    Some((
                        RawWindowHandle::AppKit(AppKitWindowHandle::new(ns_view)),
                        RawDisplayHandle::AppKit(AppKitDisplayHandle::new()),
                    ))
                }
                PILL_WINDOW_BACKEND_XLIB => {
                    let mut window = XlibWindowHandle::new(self.window_primary as _);
                    window.visual_id = self.window_secondary as _;
                    let display = XlibDisplayHandle::new(
                        NonNull::new(self.display_primary as *mut c_void),
                        self.display_secondary as _,
                    );
                    Some((
                        RawWindowHandle::Xlib(window),
                        RawDisplayHandle::Xlib(display),
                    ))
                }
                PILL_WINDOW_BACKEND_XCB => {
                    let window_id = NonZeroU32::new(self.window_primary as u32)?;
                    let mut window = XcbWindowHandle::new(window_id);
                    window.visual_id = NonZeroU32::new(self.window_secondary as u32);
                    let display = XcbDisplayHandle::new(
                        NonNull::new(self.display_primary as *mut c_void),
                        self.display_secondary as _,
                    );
                    Some((RawWindowHandle::Xcb(window), RawDisplayHandle::Xcb(display)))
                }
                PILL_WINDOW_BACKEND_WAYLAND => {
                    let surface = NonNull::new(self.window_primary as *mut c_void)?;
                    let display = NonNull::new(self.display_primary as *mut c_void)?;
                    Some((
                        RawWindowHandle::Wayland(WaylandWindowHandle::new(surface)),
                        RawDisplayHandle::Wayland(WaylandDisplayHandle::new(display)),
                    ))
                }
                _ => None,
            }
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Boundary value types must keep a stable, pointer-width-independent size.
    #[test]
    fn boundary_value_types_have_fixed_layout() {
        assert_eq!(std::mem::size_of::<FrameReport>(), 16);
        assert_eq!(std::mem::size_of::<RenderViewport>(), 16);
        assert_eq!(std::mem::size_of::<VirtualResolution>(), 8);
        assert_eq!(std::mem::size_of::<PillWindowHandleV1>(), 40);
    }

    /// Embedded viewports cannot extend beyond their native surface.
    #[test]
    fn render_viewport_clamps_to_surface_bounds() {
        assert_eq!(
            RenderViewport::new(80, 40, 50, 70).clamped_to(100, 90),
            Some(RenderViewport::new(80, 40, 20, 50))
        );
        assert_eq!(
            RenderViewport::new(100, 0, 20, 20).clamped_to(100, 90),
            None
        );
    }

    /// Only finite, strictly positive dimensions can drive the projection.
    #[test]
    fn virtual_resolution_rejects_degenerate_dimensions() {
        assert!(VirtualResolution::new(800.0, 600.0).is_valid());
        assert!(!VirtualResolution::new(0.0, 600.0).is_valid());
        assert!(!VirtualResolution::new(800.0, f32::NAN).is_valid());
        assert!(!VirtualResolution::new(-1.0, 600.0).is_valid());
    }

    /// The headless descriptor names no window but still carries its layout guard.
    #[test]
    fn headless_window_descriptor_is_recognizable() {
        let descriptor = PillWindowHandleV1::none();
        assert!(descriptor.has_expected_layout());
        assert!(!descriptor.describes_window());
    }

    /// A descriptor produced by a different layout is rejected instead of read.
    #[test]
    fn window_descriptor_layout_guard_rejects_foreign_sizes() {
        let mut descriptor = PillWindowHandleV1::none();
        descriptor.struct_size = 8;
        assert!(!descriptor.has_expected_layout());
    }

    /// Win32 handles survive a flatten/rebuild round trip unchanged.
    #[cfg(all(feature = "window-handle", target_os = "windows"))]
    #[test]
    fn win32_window_handles_round_trip() {
        use raw_window_handle::{RawDisplayHandle, RawWindowHandle, Win32WindowHandle};
        use std::num::NonZeroIsize;

        let mut window = Win32WindowHandle::new(NonZeroIsize::new(0x1234).unwrap());
        window.hinstance = NonZeroIsize::new(0x5678);
        let descriptor = PillWindowHandleV1::from_raw_handles(
            RawWindowHandle::Win32(window),
            RawDisplayHandle::Windows(raw_window_handle::WindowsDisplayHandle::new()),
        )
        .expect("win32 handles are describable");

        assert_eq!(descriptor.backend, PILL_WINDOW_BACKEND_WIN32);
        let (rebuilt_window, _display) = descriptor
            .to_raw_handles()
            .expect("win32 descriptor rebuilds");
        match rebuilt_window {
            RawWindowHandle::Win32(rebuilt) => {
                assert_eq!(rebuilt.hwnd.get(), 0x1234);
                assert_eq!(rebuilt.hinstance.map(NonZeroIsize::get), Some(0x5678));
            }
            other => panic!("unexpected window handle: {other:?}"),
        }
    }

    /// A headless descriptor never rebuilds into platform handles.
    #[cfg(feature = "window-handle")]
    #[test]
    fn headless_descriptor_has_no_raw_handles() {
        assert!(PillWindowHandleV1::none().to_raw_handles().is_none());
    }
}
