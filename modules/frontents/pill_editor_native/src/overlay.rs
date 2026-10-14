//! The editor's composition overlay: the UI layer that draws above the
//! scene's own child window (Stage 21, D24).
//!
//! # Responsibilities
//!
//! - [`OverlaySurface`]: the DirectComposition target and visual over the
//!   editor window, the `wgpu` surface drawn through the visual, the
//!   per-monitor size that keeps the swapchain in step with the window, and
//!   the content signature that skips the GPU work of an unchanged overlay.
//! - [`work_area`]: the monitor's usable rectangle, which the popup placement
//!   flips and slides inside.
//!
//! # Design
//!
//! A flip-model swapchain on the window's `HWND` cannot be transparent over
//! the scene's child window, but a DirectComposition visual on a `topmost`
//! target can: the visual is composited by the desktop compositor above every
//! child surface, so the overlay's pixels are per-pixel alpha over the scene.
//! `wgpu` draws into it through `SurfaceTargetUnsafe::CompositionVisual`,
//! which its DX12 backend implements by setting the swapchain as the visual's
//! content - so the composition tree must be committed after every configure,
//! and the visual must outlive the surface.
//!
//! Everything here is the executable's: the DirectComposition device is
//! created through the `windows` crate in the binary only, never in a shared
//! dylib, whose every hot patch would otherwise import `dcomp.dll`. The Linux
//! half of D24 (Stage 31) replaces this module's Windows half behind the same
//! [`OverlaySurface`] interface; nothing above it knows the overlay is a
//! composition visual rather than, say, an `xdg_surface`.
//!
//! The overlay draws the shell's `overlay` list with `PreMultiplied`
//! composition: `pill_ui`'s blend writes straight-alpha colour into a
//! transparent target, which leaves premultiplied values behind, exactly what
//! the compositor expects to multiply back out.

// Standard library
use std::sync::Arc;

// External crates
use pill_ui::draw::{Color, DrawList};
use pill_ui_wgpu::UiRenderer;
use winit::window::Window;

// Current crate
use crate::placement::WorkArea;

/// Why an overlay could not be created; the executable logs it once and
/// falls back to the renderer's UI pass.
pub type OverlayError = String;

// =============================================================================
// OverlaySurface
// =============================================================================

/// The composition overlay over the editor window.
pub struct OverlaySurface {
    /// The DirectComposition tree the surface is set on; kept alive (and
    /// committed) for the surface's whole life.
    #[cfg(windows)]
    composition: composition::CompositionTree,
    /// The overlay's swapchain, drawn through the composition visual.
    surface: wgpu::Surface<'static>,
    /// The swapchain's configuration: the window's client area in physical
    /// pixels, the window's format, premultiplied composition.
    configuration: wgpu::SurfaceConfiguration,
    /// The monitor scale the overlay's logical pixels are drawn at (D30).
    scale_factor: f32,
    /// The last content drawn, so an unchanged overlay costs no GPU work
    /// (D25).
    signature: Option<u64>,
    /// Overlay frames presented since startup; the suite reads it.
    frames: u64,
}

impl OverlaySurface {
    /// Create the composition overlay for `window`.
    ///
    /// Fails - with the reason the executable logs - when the adapter is not
    /// DX12, when the compositor is unavailable, or when the overlay's
    /// swapchain cannot carry the window's format and premultiplied alpha.
    pub fn new(
        instance: &wgpu::Instance,
        adapter: &wgpu::Adapter,
        device: &wgpu::Device,
        window: Arc<Window>,
        format: wgpu::TextureFormat,
        size: (u32, u32),
        scale_factor: f32,
    ) -> Result<OverlaySurface, OverlayError> {
        // Step 1: the composition visual comes first - the surface is created
        // from it, and wgpu keeps the visual alive as long as the surface.
        if adapter.get_info().backend != wgpu::Backend::Dx12 {
            return Err(format!(
                "the overlay needs the DX12 backend (CompositionVisual is DX12-only), the adapter is {:?}",
                adapter.get_info().backend
            ));
        }
        let composition = composition::CompositionTree::new(&window)?;
        // Step 2: the surface, from the visual's COM pointer.
        // SAFETY: the visual is a live `IDCompositionVisual` owned by
        // `composition`, which outlives the surface, and the pointer is its
        // interface pointer, which is what the DX12 backend borrows and clones.
        let surface = unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CompositionVisual(
                composition.visual_pointer(),
            ))
        }
        .map_err(|error| format!("the composition surface was refused: {error}"))?;
        // Step 3: the swapchain must carry the window's format (the overlay
        // and the panels share one `UiRenderer`) and premultiplied
        // composition (the visual is composited over the scene).
        let capabilities = surface.get_capabilities(adapter);
        if !capabilities.formats.contains(&format) {
            return Err(format!(
                "the overlay surface does not offer the window's format {format:?}"
            ));
        }
        if !capabilities
            .alpha_modes
            .contains(&wgpu::CompositeAlphaMode::PreMultiplied)
        {
            return Err("the overlay surface does not offer premultiplied composition".to_string());
        }
        let configuration = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.0.max(1),
            height: size.1.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: wgpu::CompositeAlphaMode::PreMultiplied,
            view_formats: vec![],
        };
        surface.configure(device, &configuration);
        // Step 4: the swapchain became the visual's content inside
        // `configure`; the tree only shows it once committed.
        composition.commit()?;
        Ok(OverlaySurface {
            #[cfg(windows)]
            composition,
            surface,
            configuration,
            scale_factor: scale_factor.max(0.01),
            signature: None,
            frames: 0,
        })
    }

    /// The monitor scale the overlay's logical pixels are drawn at.
    ///
    /// Follow a monitor change (D30): the shell's logical pixels stay the
    /// same, the swapchain's physical ones do not.
    pub fn set_scale_factor(&mut self, scale_factor: f32) {
        self.scale_factor = scale_factor.max(0.01);
    }

    /// Resize the swapchain to `size` physical pixels (the window's client
    /// area) and commit the new content.
    pub fn resize(&mut self, device: &wgpu::Device, size: (u32, u32)) {
        let width = size.0.max(1);
        let height = size.1.max(1);
        if self.configuration.width == width && self.configuration.height == height {
            return;
        }
        self.configuration.width = width;
        self.configuration.height = height;
        self.surface.configure(device, &self.configuration);
        #[cfg(windows)]
        if let Err(error) = self.composition.commit() {
            pill_core::warn!(
                target: pill_core::telemetry::telemetry_target::ECS,
                "[editor] overlay commit failed after resize: {error}"
            );
        }
    }

    /// The overlay's frames presented since startup.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Draw `list` over the scene and present it.
    ///
    /// An empty list clears the overlay to fully transparent and presents
    /// that, so the previous frame's content disappears; an unchanged list
    /// is not drawn at all (D25). Returns whether GPU work happened.
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        renderer: &mut UiRenderer,
        list: &DrawList,
    ) -> Result<bool, OverlayError> {
        let signature = content_signature(list);
        if list.is_empty() && self.signature.is_none() {
            return Ok(false);
        }
        if self.signature == Some(signature) {
            return Ok(false);
        }
        let frame = self
            .surface
            .get_current_texture()
            .map_err(|error| format!("the overlay swapchain is unavailable: {error}"))?;
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("pill_editor overlay"),
        });
        renderer.render(
            device,
            queue,
            &mut encoder,
            &view,
            (self.configuration.width, self.configuration.height),
            self.scale_factor,
            list,
            // Fully transparent: the visual is composited over the scene, so
            // whatever the list leaves out must show the scene through.
            Some(Color::TRANSPARENT),
        );
        queue.submit(Some(encoder.finish()));
        frame.present();
        self.signature = Some(signature);
        self.frames += 1;
        Ok(true)
    }
}

/// A hash of everything a draw list would draw, for the redraw-on-change
/// gate (D25).
///
/// Vertices, indices and clips all feed it, so any visible change - a moved
/// badge, a re-coloured toast, an appearing preview - changes it, while two
/// frames of identical content hash the same and cost no GPU work.
fn content_signature(list: &DrawList) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |value: u64| {
        hash ^= value;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    };
    mix(list.vertices.len() as u64);
    mix(list.indices.len() as u64);
    mix(list.commands.len() as u64);
    for vertex in &list.vertices {
        mix(vertex.position.x.to_bits() as u64);
        mix(vertex.position.y.to_bits() as u64);
        mix(vertex.uv.x.to_bits() as u64);
        mix(vertex.uv.y.to_bits() as u64);
        mix(vertex.color.a.to_bits() as u64);
    }
    for index in &list.indices {
        mix(*index as u64);
    }
    hash
}

// =============================================================================
// Work area
// =============================================================================

/// The usable rectangle of the monitor `window` is on, in physical screen
/// pixels: where a popup may open without crossing the taskbar.
#[cfg(windows)]
pub fn work_area(window: &Window) -> Option<WorkArea> {
    let hwnd = hwnd_of(window)?;
    // SAFETY: the handle is a live window handle, and the call writes only
    // into the local `MONITORINFO` it is given, whose size it is told.
    unsafe {
        use windows::Win32::Graphics::Gdi::{
            GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
        };
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        if monitor.is_invalid() {
            return None;
        }
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return None;
        }
        let work = info.rcWork;
        Some(WorkArea::new(
            work.left,
            work.top,
            (work.right - work.left).max(0) as u32,
            (work.bottom - work.top).max(0) as u32,
        ))
    }
}

/// The monitor's rectangle as winit reports it; Linux (Stage 31) has no
/// separate work area here, so the popup slides inside the monitor.
#[cfg(not(windows))]
pub fn work_area(window: &Window) -> Option<WorkArea> {
    let monitor = window.current_monitor()?;
    let position = monitor.position();
    let size = monitor.size();
    Some(WorkArea::new(
        position.x,
        position.y,
        size.width,
        size.height,
    ))
}

/// The window's `HWND`, as the composition target and the monitor lookup
/// need it.
#[cfg(windows)]
pub fn hwnd_of(window: &Window) -> Option<windows::Win32::Foundation::HWND> {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = window.window_handle().ok()?;
    match handle.as_raw() {
        RawWindowHandle::Win32(win32) => Some(windows::Win32::Foundation::HWND(
            win32.hwnd.get() as *mut std::ffi::c_void
        )),
        _ => None,
    }
}

// =============================================================================
// DirectComposition
// =============================================================================

/// The DirectComposition half of the overlay, in the Windows API's terms.
#[cfg(windows)]
mod composition {
    // Standard library
    use std::ffi::c_void;

    // External crates
    use windows::core::IUnknown;
    use windows::Win32::Graphics::DirectComposition::{
        DCompositionCreateDevice3, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
    };
    use winit::window::Window;

    // Current crate
    use crate::overlay::hwnd_of;

    /// One window's composition tree: the device, the `topmost` target on the
    /// window, and the visual the overlay's swapchain is set as the content
    /// of.
    pub struct CompositionTree {
        /// The composition device; every tree change is committed through it.
        device: IDCompositionDevice,
        /// The target on the window. Held so the target outlives the visual:
        /// dropping it would tear the tree down.
        _target: IDCompositionTarget,
        /// The visual the overlay's swapchain becomes the content of.
        visual: IDCompositionVisual,
    }

    impl CompositionTree {
        /// Create the tree over `window`, with the visual on top of the
        /// window's child surfaces.
        pub fn new(window: &Window) -> Result<CompositionTree, String> {
            let hwnd = hwnd_of(window).ok_or("the window has no Win32 handle")?;
            // SAFETY: `DCompositionCreateDevice3` is given no rendering
            // device (the composition device makes its own), and returns a
            // fresh interface whose reference the returned value owns.
            let device: IDCompositionDevice =
                unsafe { DCompositionCreateDevice3(None::<&IUnknown>) }
                    .map_err(|error| format!("DCompositionCreateDevice3 failed: {error}"))?;
            // SAFETY: the handle is live and the device is alive; `true` asks
            // for the topmost target, which is what puts the visual above the
            // window's child windows.
            let target = unsafe { device.CreateTargetForHwnd(hwnd, true) }
                .map_err(|error| format!("CreateTargetForHwnd failed: {error}"))?;
            // SAFETY: the device is alive; the visual is a fresh interface the
            // returned value owns.
            let visual = unsafe { device.CreateVisual() }
                .map_err(|error| format!("CreateVisual failed: {error}"))?;
            // SAFETY: the target and the visual are alive and belong to this
            // device, which is the order `SetRoot` requires.
            unsafe { target.SetRoot(&visual) }
                .map_err(|error| format!("SetRoot failed: {error}"))?;
            let tree = CompositionTree {
                device,
                _target: target,
                visual,
            };
            tree.commit()?;
            Ok(tree)
        }

        /// The visual's COM interface pointer, which `wgpu`'s DX12 backend
        /// borrows to build the overlay surface.
        pub fn visual_pointer(&self) -> *mut c_void {
            windows::core::Interface::as_raw(&self.visual)
        }

        /// Publish the tree: the composition device only shows a change once
        /// it is committed.
        pub fn commit(&self) -> Result<(), String> {
            // SAFETY: the device is alive for as long as the tree is.
            unsafe { self.device.Commit() }.map_err(|error| format!("Commit failed: {error}"))
        }
    }
}
