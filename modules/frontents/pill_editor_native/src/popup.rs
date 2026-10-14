//! Platform popups: the shell's menus, context menus and tooltips as
//! windows of their own, so they may extend past the editor window
//! (Stage 21, D23).
//!
//! # Responsibilities
//!
//! - [`PopupHost`]: create, move, draw and close the popup windows the shell
//!   asks for each frame, one owned undecorated top-level window per popup,
//!   each with its own `wgpu` surface.
//! - Report the popups the platform closed itself - an outside click that
//!   took the focus away, an Escape, a window-manager close - so the shell
//!   clears the state it opened them from.
//! - Report the pointer over a popup, in the popup's own logical pixels: a
//!   popup is a window, so its input never arrives as an engine
//!   [`pill_engine_core::InputEvent`].
//!
//! # Design
//!
//! A popup is a separate top-level window owned by the editor window
//! (`WS_POPUP`, undecorated), which is what lets it cross the editor
//! window's edge; the flip and slide that keep it inside the monitor's work
//! area live in [`crate::placement`], platform-free. The popup's content is
//! the shell's own draw list, in the popup's logical pixels, so nothing here
//! knows what a menu is.
//!
//! Popups are created and moved once per frame, from the shell's output of
//! the previous frame, and drawn from the same list - so a popup's content
//! and its window never disagree about the size. The windows are created
//! through the event loop, which the frame itself does not have; that is why
//! [`PopupHost::sync`] is called from the loop's `about_to_wait`.

// Standard library
use std::sync::Arc;

// External crates
use pill_core::math::Vector2f;
use pill_editor_api::{EditorPopup, EditorPopupPointer, EditorRect};
use pill_ui::draw::{Color, DrawList};
use pill_ui_wgpu::UiRenderer;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

// Current crate
use crate::overlay::work_area;
use crate::placement::{place_popup, PopupPlacement};

/// The GPU state a popup needs to exist; the executable owns it, so it is
/// handed in rather than owned here.
pub struct PopupGpu<'a> {
    /// The wgpu instance the popup surfaces come from.
    pub instance: &'a wgpu::Instance,
    /// The adapter the surfaces are checked against.
    pub adapter: &'a wgpu::Adapter,
    /// The device the surfaces are configured on.
    pub device: &'a wgpu::Device,
    /// The editor window's surface format: the popups share the one
    /// `UiRenderer` the executable draws its UI with.
    pub format: wgpu::TextureFormat,
}

// =============================================================================
// PopupHost
// =============================================================================

/// Every popup window the shell currently has open.
#[derive(Default)]
pub struct PopupHost {
    /// The open popups, in creation order.
    popups: Vec<PopupWindow>,
    /// Pointer states collected since the executable last read them.
    pointers: Vec<EditorPopupPointer>,
    /// Popups opened since startup; the suite reads it.
    opened: u64,
}

impl PopupHost {
    /// An empty host.
    pub fn new() -> PopupHost {
        PopupHost::default()
    }

    /// How many popups are open.
    pub fn len(&self) -> usize {
        self.popups.len()
    }

    /// Whether no popup is open.
    pub fn is_empty(&self) -> bool {
        self.popups.is_empty()
    }

    /// How many popups were opened since startup.
    pub fn opened(&self) -> u64 {
        self.opened
    }

    /// Whether `window_id` is one of the popups.
    pub fn is_popup(&self, window_id: WindowId) -> bool {
        self.popups
            .iter()
            .any(|popup| popup.window.id() == window_id)
    }

    /// Take the pointer states collected since the last call.
    pub fn take_pointers(&mut self) -> Vec<EditorPopupPointer> {
        std::mem::take(&mut self.pointers)
    }

    /// Queue a pointer for one popup through the channel the window's own
    /// events fill (Stage 21). The scripted suite cannot move the real
    /// cursor, so it hands the popup's pointer over here; `false` when no
    /// popup with that id is open.
    pub fn push_pointer(&mut self, id: &str, position: Vector2f, released: bool) -> bool {
        if !self.popups.iter().any(|popup| popup.id == id) {
            return false;
        }
        self.pointers.push(EditorPopupPointer {
            id: id.to_string(),
            position,
            released,
        });
        true
    }

    /// Dismiss one popup the way the platform does (Stage 21): the state an
    /// outside click, a lost focus or an Escape leaves behind. The next
    /// `sync` drops it and reports the id, so the shell clears its state
    /// exactly as it would on its own dismissal. `false` when no popup with
    /// that id is open.
    pub fn dismiss(&mut self, id: &str) -> bool {
        let Some(popup) = self.popups.iter_mut().find(|popup| popup.id == id) else {
            return false;
        };
        popup.closed = true;
        true
    }

    /// Close every popup, for shutdown.
    pub fn close_all(&mut self) {
        self.popups.clear();
        self.pointers.clear();
    }

    /// Dismiss every open popup the way the platform does when another
    /// window takes the focus (Stage 21): a click on the editor's own
    /// window, an Alt+Tab. Unlike [`PopupHost::close_all`] the ids are
    /// reported by the next `sync`, so the shell clears the state behind
    /// them instead of asking for them again.
    pub fn dismiss_all(&mut self) {
        for popup in &mut self.popups {
            popup.closed = true;
        }
    }

    /// Open, move and close popups so the open set matches `requested`.
    ///
    /// Returns the ids of the popups that closed this frame, whether the
    /// shell asked (by leaving them out) or the platform did (an outside
    /// click, an Escape); the shell clears its state from that list.
    pub fn sync(
        &mut self,
        event_loop: &ActiveEventLoop,
        gpu: &PopupGpu<'_>,
        parent: &Window,
        requested: &[EditorPopup],
    ) -> Vec<String> {
        let mut closed = self.close_unrequested(requested);
        for request in requested {
            if request.id.is_empty() {
                continue;
            }
            match self.popups.iter().position(|popup| popup.id == request.id) {
                Some(index) => self.popups[index].follow(request, parent),
                None => match PopupWindow::open(event_loop, gpu, parent, request) {
                    Ok(popup) => {
                        self.opened += 1;
                        pill_core::info!(
                            target: pill_core::telemetry::telemetry_target::ECS,
                            "[editor] popup opened id={} size=({}, {}) requested=({}, {}) placed=({}, {}) flipped={} slid={}",
                            popup.id,
                            popup.configuration.width,
                            popup.configuration.height,
                            request.rect.x as i32,
                            request.rect.y as i32,
                            popup.placement.origin.0,
                            popup.placement.origin.1,
                            popup.placement.flipped,
                            popup.placement.slid,
                        );
                        self.popups.push(popup);
                    }
                    Err(error) => pill_core::warn!(
                        target: pill_core::telemetry::telemetry_target::ECS,
                        "[editor] popup `{}` did not open: {error}",
                        request.id
                    ),
                },
            }
        }
        // A popup that closed itself (Escape, a lost focus) is dropped here,
        // and its id is what the shell needs to clear its state.
        let mut index = 0;
        while index < self.popups.len() {
            if self.popups[index].closed {
                closed.push(self.popups[index].id.clone());
                self.popups.remove(index);
            } else {
                index += 1;
            }
        }
        closed
    }

    /// Drop every popup the shell no longer asks for.
    fn close_unrequested(&mut self, requested: &[EditorPopup]) -> Vec<String> {
        let mut closed = Vec::new();
        self.popups.retain(|popup| {
            let wanted = requested.iter().any(|request| request.id == popup.id);
            if !wanted {
                closed.push(popup.id.clone());
            }
            wanted
        });
        closed
    }

    /// Draw every open popup with the shared renderer and present it.
    ///
    /// `background` fills whatever the popup's own list leaves out, so a
    /// stale frame is never visible; a popup whose content did not change is
    /// not drawn again (D25).
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        renderer: &mut UiRenderer,
        background: Color,
    ) {
        for popup in &mut self.popups {
            if let Err(error) = popup.render(device, queue, renderer, background) {
                pill_core::warn!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    "[editor] popup `{}` did not draw: {error}",
                    popup.id
                );
            }
        }
    }

    /// Handle one event of one popup window; `false` when the window is not a
    /// popup.
    pub fn window_event(&mut self, window_id: WindowId, event: &WindowEvent) -> bool {
        let Some(popup) = self
            .popups
            .iter_mut()
            .find(|popup| popup.window.id() == window_id)
        else {
            return false;
        };
        match event {
            WindowEvent::CloseRequested => popup.closed = true,
            // A popup that had the focus and lost it - a click on another
            // window, an Alt+Tab - is dismissed, the way every menu on the
            // platform is. A popup that never had the focus cannot lose it,
            // so the `Focused(false)` a window reports around its creation
            // never closes it.
            WindowEvent::Focused(false) if popup.ever_focused => popup.closed = true,
            WindowEvent::Focused(true) => popup.ever_focused = true,
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed
                    && matches!(&event.logical_key, Key::Named(NamedKey::Escape)) =>
            {
                popup.closed = true;
            }
            WindowEvent::CursorMoved { position, .. } => {
                let scale_factor = popup.window.scale_factor().max(0.01) as f32;
                popup.cursor = Vector2f::new(
                    position.x as f32 / scale_factor,
                    position.y as f32 / scale_factor,
                );
                self.pointers.push(EditorPopupPointer {
                    id: popup.id.clone(),
                    position: popup.cursor,
                    released: false,
                });
            }
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } => self.pointers.push(EditorPopupPointer {
                id: popup.id.clone(),
                position: popup.cursor,
                released: true,
            }),
            _ => {}
        }
        true
    }
}

// =============================================================================
// PopupWindow
// =============================================================================

/// One open popup: its window, its swapchain and what it draws.
struct PopupWindow {
    /// The shell's identity for this popup.
    id: String,
    /// The popup's own top-level window.
    window: Arc<Window>,
    /// Its swapchain.
    surface: wgpu::Surface<'static>,
    /// Its configuration: the popup's size in physical pixels.
    configuration: wgpu::SurfaceConfiguration,
    /// Where the platform put it, in physical screen pixels.
    placement: PopupPlacement,
    /// What to draw, in the popup's own logical pixels.
    draw: DrawList,
    /// The content signature last presented (D25).
    signature: Option<u64>,
    /// The last pointer position, in the popup's own logical pixels.
    cursor: Vector2f,
    /// Whether the popup was ever focused; see [`PopupHost::window_event`].
    ever_focused: bool,
    /// Set by the popup's own events: the host drops it at the next sync.
    closed: bool,
}

impl PopupWindow {
    /// Open one popup for `request`, anchored in `parent`'s client area and
    /// flipped inside its monitor's work area.
    fn open(
        event_loop: &ActiveEventLoop,
        gpu: &PopupGpu<'_>,
        parent: &Window,
        request: &EditorPopup,
    ) -> Result<PopupWindow, String> {
        let scale_factor = parent.scale_factor().max(0.01) as f32;
        let size = physical_size(request.rect, scale_factor);
        let anchor = anchor_on_screen(parent, request.rect, scale_factor);
        let work = work_area(parent).ok_or("the window's monitor has no work area")?;
        let placement = place_popup(anchor, size, work);
        let attributes = popup_attributes(parent, size, placement);
        let window = Arc::new(
            event_loop
                .create_window(attributes)
                .map_err(|error| format!("the popup window was refused: {error}"))?,
        );
        let surface = gpu
            .instance
            .create_surface(Arc::clone(&window))
            .map_err(|error| format!("the popup surface was refused: {error}"))?;
        let capabilities = surface.get_capabilities(gpu.adapter);
        if !capabilities.formats.contains(&gpu.format) {
            return Err(format!(
                "the popup surface does not offer the window's format {:?}",
                gpu.format
            ));
        }
        let configuration = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: gpu.format,
            width: size.0,
            height: size.1,
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            // A popup window's surface only offers opaque composition; the
            // popup's own background covers its whole area.
            alpha_mode: capabilities.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(gpu.device, &configuration);
        Ok(PopupWindow {
            id: request.id.clone(),
            window,
            surface,
            configuration,
            placement,
            draw: request.draw.clone(),
            signature: None,
            cursor: Vector2f::ZERO,
            ever_focused: false,
            closed: false,
        })
    }

    /// Follow the shell's request for this popup: resize and reposition it
    /// when the rectangle changed, and take the new content.
    fn follow(&mut self, request: &EditorPopup, parent: &Window) {
        let scale_factor = parent.scale_factor().max(0.01) as f32;
        let size = physical_size(request.rect, scale_factor);
        if (self.configuration.width, self.configuration.height) != size {
            self.configuration.width = size.0;
            self.configuration.height = size.1;
            let _ = self
                .window
                .request_inner_size(winit::dpi::PhysicalSize::new(size.0, size.1));
            // The surface follows the window in `render`, which reconfigures
            // from the window's own size before it draws.
        }
        let anchor = anchor_on_screen(parent, request.rect, scale_factor);
        if let Some(work) = work_area(parent) {
            let placement = place_popup(anchor, size, work);
            if placement != self.placement {
                // NOTE: asserted on by the editor's end-to-end suite: the
                // flip at the monitor's edge is only observable from here.
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    "[editor] popup placed id={} requested=({}, {}) placed=({}, {}) flipped={} slid={}",
                    self.id,
                    anchor.0,
                    anchor.1,
                    placement.origin.0,
                    placement.origin.1,
                    placement.flipped,
                    placement.slid,
                );
                self.window
                    .set_outer_position(winit::dpi::PhysicalPosition::new(
                        placement.origin.0,
                        placement.origin.1,
                    ));
                self.placement = placement;
            }
        }
        self.draw = request.draw.clone();
    }

    /// Reconfigure the swapchain for the popup's current size.
    fn reconfigure(&mut self, device: &wgpu::Device, size: (u32, u32)) {
        let width = size.0.max(1);
        let height = size.1.max(1);
        if (self.configuration.width, self.configuration.height) == (width, height) {
            return;
        }
        self.configuration.width = width;
        self.configuration.height = height;
        self.surface.configure(device, &self.configuration);
    }

    /// Draw this popup's list and present it, unless nothing changed.
    fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        renderer: &mut UiRenderer,
        background: Color,
    ) -> Result<(), String> {
        let signature = content_signature(&self.draw, self.configuration.width);
        if self.signature == Some(signature) {
            return Ok(());
        }
        // The window may have been resized under us - a requested size that
        // arrived late, or a monitor change (D30): the swapchain follows the
        // window before it is drawn into.
        let size = self.window.inner_size();
        self.reconfigure(device, (size.width, size.height));
        let frame = self
            .surface
            .get_current_texture()
            .map_err(|error| format!("the popup swapchain is unavailable: {error}"))?;
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("pill_editor popup"),
        });
        renderer.render(
            device,
            queue,
            &mut encoder,
            &view,
            (self.configuration.width, self.configuration.height),
            self.window.scale_factor().max(0.01) as f32,
            &self.draw,
            Some(background),
        );
        queue.submit(Some(encoder.finish()));
        frame.present();
        self.signature = Some(signature);
        Ok(())
    }
}

// =============================================================================
// Placement helpers
// =============================================================================

/// A popup's size in physical pixels.
fn physical_size(rect: EditorRect, scale_factor: f32) -> (u32, u32) {
    (
        (rect.width * scale_factor).round().max(1.0) as u32,
        (rect.height * scale_factor).round().max(1.0) as u32,
    )
}

/// Where a popup's top-left is asked for, in physical screen pixels.
///
/// The shell names it in the editor window's client area; `inner_position` is
/// where that area starts on screen, so a popup opened at the client area's
/// own origin lands exactly on it.
fn anchor_on_screen(parent: &Window, rect: EditorRect, scale_factor: f32) -> (i32, i32) {
    let origin = parent
        .inner_position()
        .map(|position| (position.x, position.y))
        .unwrap_or((0, 0));
    (
        origin.0 + (rect.x * scale_factor).round() as i32,
        origin.1 + (rect.y * scale_factor).round() as i32,
    )
}

/// The window attributes of one popup: an undecorated, unresizable,
/// non-activating top-level window owned by the editor window, so it is
/// above it, leaves the taskbar alone and never takes the editor's focus
/// while it opens.
fn popup_attributes(
    parent: &Window,
    size: (u32, u32),
    placement: PopupPlacement,
) -> winit::window::WindowAttributes {
    let attributes = Window::default_attributes()
        .with_title("pill editor popup")
        .with_decorations(false)
        .with_resizable(false)
        .with_inner_size(winit::dpi::PhysicalSize::new(size.0, size.1))
        .with_position(winit::dpi::PhysicalPosition::new(
            placement.origin.0,
            placement.origin.1,
        ))
        .with_active(false);
    owned_popup(attributes, parent)
}

/// Own the popup to the editor window on Windows; on other platforms (Stage
/// 31 opens an `xdg_popup` for the same request) it stays a plain
/// undecorated top-level window.
#[cfg(windows)]
fn owned_popup(
    attributes: winit::window::WindowAttributes,
    parent: &Window,
) -> winit::window::WindowAttributes {
    use winit::platform::windows::WindowAttributesExtWindows;
    match crate::overlay::hwnd_of(parent) {
        Some(hwnd) => attributes.with_owner_window(hwnd.0 as isize),
        None => attributes,
    }
}

/// The non-Windows half: a plain top-level window until Stage 31 gives it a
/// platform popup of its own.
#[cfg(not(windows))]
fn owned_popup(
    attributes: winit::window::WindowAttributes,
    _parent: &Window,
) -> winit::window::WindowAttributes {
    attributes
}

/// A hash of what a popup would draw, for the redraw-on-change gate (D25).
fn content_signature(list: &DrawList, width: u32) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |value: u64| {
        hash ^= value;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    };
    mix(width as u64);
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
