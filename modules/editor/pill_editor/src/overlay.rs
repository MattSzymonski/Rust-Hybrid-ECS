//! The editor's composition overlay: the UI layer drawn above the scene's
//! own child window.
//!
//! # Responsibilities
//!
//! - [`OverlaySurface`]: the DirectComposition target and visual over the
//!   editor window, the `wgpu` surface drawn through it, the size that keeps
//!   the swapchain in step with the window, and the empty-frame gate.
//!
//! # Design
//!
//! A flip-model swapchain on the window's `HWND` cannot be transparent over
//! the scene's child window, but a DirectComposition visual on a `topmost`
//! target can: the desktop compositor draws it above every child surface, so
//! the overlay's pixels are per-pixel alpha over the scene. `wgpu` draws into
//! it through `SurfaceTargetUnsafe::CompositionVisual`, which its DX12
//! backend implements by setting the swapchain as the visual's content - so
//! the tree is committed after every configure and the visual outlives the
//! surface.
//!
//! The content is the shell's `EguiShellFrame`, drawn premultiplied with
//! `egui-wgpu`'s renderer on the editor's own device, which is what the
//! compositor multiplies back out.
//!
//! The DirectComposition device is created through the `windows` crate in the
//! binary only, never in a shared dylib, whose every hot patch would otherwise
//! import `dcomp.dll`.

// Standard library
use std::sync::Arc;

// External crates
use pill_editor_core::egui::epaint::ClippedPrimitive;
use pill_editor_core::egui::TexturesDelta;
use winit::window::Window;

/// Why an overlay could not be created; the executable logs it once and
/// keeps running without the UI layer.
pub type OverlayError = String;

/// Where the UI's pixels go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverlayMode {
    /// The composition visual above the scene: transparent where the shell
    /// drew nothing, so the scene shows through the UI's gaps.
    Composition,
    /// The window's own surface, with no composition (`PILL_EDITOR_NO_OVERLAY`
    /// or a failed overlay): the UI is opaque, and a scene child window -
    /// always above its parent - hides whatever the UI draws under it.
    Window,
}

/// The colour the window posture clears to: a dark neutral, so an uncovered
/// region of the window is not black.
const WINDOW_CLEAR: wgpu::Color = wgpu::Color {
    r: 0.09,
    g: 0.10,
    b: 0.12,
    a: 1.0,
};

// =============================================================================
// OverlaySurface
// =============================================================================

/// The composition overlay over the editor window.
pub struct OverlaySurface {
    /// The posture this surface was created in.
    mode: OverlayMode,
    /// The DirectComposition tree the surface is set on; kept alive (and
    /// committed) for the surface's whole life. Only the composition posture
    /// has one.
    #[cfg(windows)]
    composition: Option<composition::CompositionTree>,
    /// The overlay's swapchain, drawn through the composition visual.
    surface: wgpu::Surface<'static>,
    /// The swapchain's configuration: the window's client area in physical
    /// pixels, the editor's format, the posture's composition mode.
    configuration: wgpu::SurfaceConfiguration,
    /// Whether the last frame drew content, so the first blank frame after
    /// content clears the swapchain and later blanks cost no GPU work.
    drew_content: bool,
    /// Overlay frames presented since startup.
    frames: u64,
}

impl OverlaySurface {
    /// Create the composition overlay for `window`.
    ///
    /// Fails - with the reason the executable logs - when the adapter is not
    /// DX12, when the compositor is unavailable, or when the overlay's
    /// swapchain cannot carry the given format and premultiplied alpha.
    pub fn new(
        instance: &wgpu::Instance,
        adapter: &wgpu::Adapter,
        device: &wgpu::Device,
        window: Arc<Window>,
        format: wgpu::TextureFormat,
        size: (u32, u32),
        _scale_factor: f32,
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
        // Step 3: the swapchain must carry the editor's format and
        // premultiplied composition (the visual is composited over the scene).
        let capabilities = surface.get_capabilities(adapter);
        if !capabilities.formats.contains(&format) {
            return Err(format!(
                "the overlay surface does not offer the editor's format {format:?}"
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
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        surface.configure(device, &configuration);
        // Step 4: the swapchain became the visual's content inside
        // `configure`; the tree only shows it once committed.
        composition.commit()?;
        Ok(OverlaySurface {
            mode: OverlayMode::Composition,
            #[cfg(windows)]
            composition: Some(composition),
            surface,
            configuration,
            drew_content: false,
            frames: 0,
        })
    }

    /// Create the overlay in the window posture: the UI's frames go to the
    /// window's own surface, with no composition and no transparency.
    ///
    /// This is the posture `PILL_EDITOR_NO_OVERLAY` asks for, and the one a
    /// failed composition overlay falls back to. It needs nothing the format
    /// does not already offer, so it works on every backend.
    pub fn new_window(
        instance: &wgpu::Instance,
        adapter: &wgpu::Adapter,
        device: &wgpu::Device,
        window: Arc<Window>,
        format: wgpu::TextureFormat,
        size: (u32, u32),
    ) -> Result<OverlaySurface, OverlayError> {
        // Step 1: a plain surface on the window.
        let surface = instance
            .create_surface(window)
            .map_err(|error| format!("the window surface was refused: {error}"))?;
        let capabilities = surface.get_capabilities(adapter);
        if !capabilities.formats.contains(&format) {
            return Err(format!(
                "the window surface does not offer the editor's format {format:?}"
            ));
        }
        // Step 2: the first composition mode the platform offers that is not
        // transparent: the window posture draws an opaque UI.
        let alpha_mode = capabilities
            .alpha_modes
            .iter()
            .copied()
            .find(|mode| *mode != wgpu::CompositeAlphaMode::PreMultiplied)
            .unwrap_or(wgpu::CompositeAlphaMode::Auto);
        let configuration = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.0.max(1),
            height: size.1.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode,
            view_formats: vec![],
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        surface.configure(device, &configuration);
        Ok(OverlaySurface {
            mode: OverlayMode::Window,
            #[cfg(windows)]
            composition: None,
            surface,
            configuration,
            drew_content: false,
            frames: 0,
        })
    }

    /// The posture this surface draws in.
    pub fn mode(&self) -> OverlayMode {
        self.mode
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
        if let Some(composition) = &self.composition {
            if let Err(error) = composition.commit() {
                pill_core::warn!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    "[editor] overlay commit failed after resize: {error}"
                );
            }
        }
    }

    /// The overlay's frames presented since startup.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Draw the shell frame's meshes over the scene and present them.
    ///
    /// An empty mesh list clears the overlay to fully transparent and
    /// presents that once, so the previous frame's content disappears; a
    /// still-empty list after that is not drawn at all. Returns whether GPU
    /// work happened.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        renderer: &mut egui_wgpu::Renderer,
        meshes: &[ClippedPrimitive],
        mut textures: TexturesDelta,
        pixels_per_point: f32,
    ) -> Result<bool, OverlayError> {
        let has_content = !meshes.is_empty();
        if !has_content && !self.drew_content {
            // Nothing is drawn, so nothing the shell uploaded is used: the
            // delta is cleared rather than left to panic in epaint's drop.
            textures.clear();
            return Ok(false);
        }
        // A suboptimal frame is still drawable; anything else means the
        // swapchain must be recreated by the frontend, which owns that path.
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            other => return Err(format!("the overlay swapchain is unavailable: {other:?}")),
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let screen_descriptor = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.configuration.width, self.configuration.height],
            pixels_per_point: pixels_per_point.max(0.01),
        };
        // Step 1: apply this frame's texture updates before drawing with
        // them. One texture may carry several deltas in one frame.
        for (texture_id, deltas) in &textures.set {
            for image_delta in deltas {
                renderer.update_texture(device, queue, *texture_id, image_delta);
            }
        }
        // Step 2: record the draw commands. `update_buffers` may produce its
        // own command buffers, which are submitted alongside the pass.
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("pill_editor overlay"),
        });
        let user_buffers =
            renderer.update_buffers(device, queue, &mut encoder, meshes, &screen_descriptor);
        {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("pill_editor overlay pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // The composition posture clears to fully transparent -
                        // the visual is composited over the scene, so whatever
                        // the meshes leave out must show the scene through. The
                        // window posture owns every pixel of its surface, so it
                        // clears to the shell's background instead.
                        load: wgpu::LoadOp::Clear(match self.mode {
                            OverlayMode::Composition => wgpu::Color::TRANSPARENT,
                            OverlayMode::Window => WINDOW_CLEAR,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            // The lifetime erasure extends the pass's borrows to `'static`;
            // it stays inside this block, so nothing outlives `view` or the
            // encoder.
            let mut pass = pass.forget_lifetime();
            renderer.render(&mut pass, meshes, &screen_descriptor);
        }
        // Step 3: release the textures the shell asked to free, then submit.
        for texture_id in &textures.free {
            renderer.free_texture(texture_id);
        }
        // Every delta is applied: an unapplied one panics in epaint's drop.
        textures.clear();
        queue.submit(user_buffers.into_iter().chain(Some(encoder.finish())));
        queue.present(frame);
        self.drew_content = has_content;
        self.frames += 1;
        Ok(true)
    }
}

// =============================================================================
// Window handle
// =============================================================================

/// The window's `HWND`, as the composition target needs it.
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
