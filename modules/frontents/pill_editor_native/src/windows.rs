//! Free-standing editor windows: the tool windows and modal dialogs the
//! shell asks for as plain data (Stage 24).
//!
//! # Responsibilities
//!
//! - [`WindowHost`]: create, draw and close the shell's windows, one real
//!   decorated top-level window per request, each with its own `wgpu`
//!   surface.
//! - Report each window's pointer, its button edges and the Enter/Escape
//!   presses a modal answers, in the window's own logical pixels, and the
//!   ids of the windows the user closed through the title bar.
//!
//! # Design
//!
//! Mirrors [`crate::popup`], with three differences: these windows are
//! decorated (the platform draws the title and the close button) and take
//! the focus, because a dialog must answer keys; they are created once and
//! then only followed, since a window's kind, title and size are fixed
//! while it stays open; and the shell sends the whole wanted set every
//! frame - a window it leaves out is closed, exactly like a popup.
//!
//! A window's content is the shell's own draw list in the window's logical
//! pixels, so nothing here knows what a panel is. The keyboard channel is
//! deliberately narrow: Enter and Escape, the two keys a modal answers.
//! Text entry inside a tool window would widen it (follow-up in the plan).

// Standard library
use std::sync::Arc;

// External crates
use pill_core::math::Vector2f;
use pill_editor_api::{
    EditorWindowKey, EditorWindowKeyKind, EditorWindowPointer, EditorWindowPointerKind,
    EditorWindowRequest, EditorWindowText,
};
use pill_engine_core::InputEvent;
use pill_input::winit_events::translate_window_event;
use pill_ui::draw::{Color, DrawList};
use pill_ui_wgpu::UiRenderer;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

// Current crate
use crate::popup::PopupGpu;

/// How far a tool window opens from its editor window's top-left corner,
/// in logical pixels; a modal centers over its owner instead.
const TOOL_WINDOW_OFFSET: f64 = 56.0;

// =============================================================================
// WindowHost
// =============================================================================

/// Every free-standing window the shell currently has open.
#[derive(Default)]
pub struct WindowHost {
    /// The open windows, in creation order.
    windows: Vec<ToolWindow>,
    /// Pointer and button events collected since the executable last read
    /// them.
    pointers: Vec<EditorWindowPointer>,
    /// Enter and Escape presses collected since the last read.
    keys: Vec<EditorWindowKey>,
    /// Committed text runs collected since the last read (Stage 25).
    texts: Vec<EditorWindowText>,
    /// Windows opened since startup; the suite reads it.
    opened: u64,
}

impl WindowHost {
    /// An empty host.
    pub fn new() -> WindowHost {
        WindowHost::default()
    }

    /// How many windows are open.
    pub fn len(&self) -> usize {
        self.windows.len()
    }

    /// Whether no window is open.
    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    /// How many windows were opened since startup.
    pub fn opened(&self) -> u64 {
        self.opened
    }

    /// Whether `window_id` is one of the free-standing windows.
    pub fn is_window(&self, window_id: WindowId) -> bool {
        self.windows
            .iter()
            .any(|window| window.window.id() == window_id)
    }

    /// Take the pointer events collected since the last call.
    pub fn take_pointers(&mut self) -> Vec<EditorWindowPointer> {
        std::mem::take(&mut self.pointers)
    }

    /// Take the key presses collected since the last call.
    pub fn take_keys(&mut self) -> Vec<EditorWindowKey> {
        std::mem::take(&mut self.keys)
    }

    /// Take the text runs collected since the last call.
    pub fn take_texts(&mut self) -> Vec<EditorWindowText> {
        std::mem::take(&mut self.texts)
    }

    /// Queue a text run for one window (the scripted test channel).
    pub fn push_text(&mut self, id: &str, text: &str) -> bool {
        if !self.windows.iter().any(|window| window.id == id) {
            return false;
        }
        self.texts.push(EditorWindowText {
            id: id.to_string(),
            text: text.to_string(),
        });
        true
    }

    /// Close every window, for shutdown.
    pub fn close_all(&mut self) {
        self.windows.clear();
        self.pointers.clear();
        self.keys.clear();
        self.texts.clear();
    }

    /// Close one window the way the user does with the title bar (the
    /// scripted test channel). Unlike [`WindowHost::close_all`] the id is
    /// reported by the next `sync`, so the shell clears its state instead
    /// of asking for the window again. `false` when no window with that id
    /// is open.
    pub fn dismiss(&mut self, id: &str) -> bool {
        let Some(window) = self.windows.iter_mut().find(|window| window.id == id) else {
            return false;
        };
        window.closed = true;
        true
    }

    /// Queue a pointer for one window through the channel the window's own
    /// events fill (Stage 24). The scripted suite cannot move the real
    /// cursor, so it hands the window's pointer over here; `false` when no
    /// window with that id is open.
    pub fn push_pointer(
        &mut self,
        id: &str,
        position: Vector2f,
        kind: EditorWindowPointerKind,
    ) -> bool {
        if !self.windows.iter().any(|window| window.id == id) {
            return false;
        }
        self.pointers.push(EditorWindowPointer {
            id: id.to_string(),
            position,
            kind,
        });
        true
    }

    /// Queue a key press for one window (the scripted test channel).
    pub fn push_key(&mut self, id: &str, key: EditorWindowKeyKind) -> bool {
        if !self.windows.iter().any(|window| window.id == id) {
            return false;
        }
        self.keys.push(EditorWindowKey {
            id: id.to_string(),
            key,
        });
        true
    }

    /// Open, move and close windows so the open set matches `requested`.
    ///
    /// Returns the ids of the windows that closed this frame, whether the
    /// shell asked (by leaving them out) or the user did (the title bar's
    /// close button); the shell clears its state from that list.
    pub fn sync(
        &mut self,
        event_loop: &ActiveEventLoop,
        gpu: &PopupGpu<'_>,
        parent: &Window,
        requested: &[EditorWindowRequest],
    ) -> Vec<String> {
        let mut closed = self.close_unrequested(requested);
        for request in requested {
            if request.id.is_empty() || request.width < 1.0 || request.height < 1.0 {
                continue;
            }
            if self.windows.iter().any(|window| window.id == request.id) {
                continue;
            }
            match ToolWindow::open(event_loop, gpu, parent, request) {
                Ok(window) => {
                    self.opened += 1;
                    pill_core::info!(
                        target: pill_core::telemetry::telemetry_target::ECS,
                        id = request.id.as_str(),
                        kind = request.kind.title(),
                        width = request.width,
                        height = request.height,
                        "[editor] window created"
                    );
                    self.windows.push(window);
                }
                Err(error) => pill_core::warn!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    id = request.id.as_str(),
                    "[editor] window `{}` did not open: {error}",
                    request.id
                ),
            }
        }
        // A window that closed itself (the title bar) is dropped here, and
        // its id is what the shell needs to clear its state.
        let mut index = 0;
        while index < self.windows.len() {
            if self.windows[index].closed {
                closed.push(self.windows[index].id.clone());
                self.windows.remove(index);
            } else {
                index += 1;
            }
        }
        closed
    }

    /// Drop every window the shell no longer asks for.
    fn close_unrequested(&mut self, requested: &[EditorWindowRequest]) -> Vec<String> {
        let mut closed = Vec::new();
        self.windows.retain(|window| {
            let wanted = requested.iter().any(|request| request.id == window.id);
            if !wanted {
                closed.push(window.id.clone());
            }
            wanted
        });
        closed
    }

    /// Draw every open window with the shared renderer and present it.
    ///
    /// The content comes from the shell's frame, matched by id; a window the
    /// frame does not name keeps its previous content until it is dropped by
    /// the next `sync`. A window whose content did not change is not drawn
    /// again (D25).
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        renderer: &mut UiRenderer,
        contents: &[EditorWindowRequest],
        background: Color,
    ) {
        for window in &mut self.windows {
            if let Some(content) = contents.iter().find(|content| content.id == window.id) {
                window.draw = content.draw.clone();
            }
            if let Err(error) = window.render(device, queue, renderer, background) {
                pill_core::warn!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    "[editor] window `{}` did not draw: {error}",
                    window.id
                );
            }
        }
    }

    /// Handle one event of one window; `false` when the window is not one of
    /// ours.
    pub fn window_event(&mut self, window_id: WindowId, event: &WindowEvent) -> bool {
        let Some(window) = self
            .windows
            .iter_mut()
            .find(|window| window.window.id() == window_id)
        else {
            return false;
        };
        match event {
            WindowEvent::CloseRequested => window.closed = true,
            WindowEvent::CursorMoved { position, .. } => {
                let scale_factor = window.window.scale_factor().max(0.01) as f32;
                window.cursor = Vector2f::new(
                    position.x as f32 / scale_factor,
                    position.y as f32 / scale_factor,
                );
                self.pointers.push(EditorWindowPointer {
                    id: window.id.clone(),
                    position: window.cursor,
                    kind: EditorWindowPointerKind::Moved,
                });
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => self.pointers.push(EditorWindowPointer {
                id: window.id.clone(),
                position: window.cursor,
                kind: match state {
                    ElementState::Pressed => EditorWindowPointerKind::Pressed,
                    ElementState::Released => EditorWindowPointerKind::Released,
                },
            }),
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed
                    && matches!(
                        &event.logical_key,
                        Key::Named(NamedKey::Enter) | Key::Named(NamedKey::Escape)
                    ) =>
            {
                let key = match &event.logical_key {
                    Key::Named(NamedKey::Escape) => EditorWindowKeyKind::Escape,
                    _ => EditorWindowKeyKind::Enter,
                };
                self.keys.push(EditorWindowKey {
                    id: window.id.clone(),
                    key,
                });
            }
            // Committed text (Stage 25): what a field inside the window
            // receives; the window's other keys stay with it until a panel
            // needs them.
            WindowEvent::KeyboardInput { .. } | WindowEvent::Ime(_) => {
                for event in translate_window_event(event) {
                    if let InputEvent::TextInput { text } = event {
                        if !text.is_empty() {
                            self.texts.push(EditorWindowText {
                                id: window.id.clone(),
                                text,
                            });
                        }
                    }
                }
            }
            _ => {}
        }
        true
    }
}

// =============================================================================
// ToolWindow
// =============================================================================

/// One open window: its platform window, its swapchain and what it draws.
struct ToolWindow {
    /// The shell's identity for this window.
    id: String,
    /// The window itself.
    window: Arc<Window>,
    /// Its swapchain.
    surface: wgpu::Surface<'static>,
    /// Its configuration: the window's size in physical pixels.
    configuration: wgpu::SurfaceConfiguration,
    /// What to draw, in the window's own logical pixels.
    draw: DrawList,
    /// The content signature last presented (D25).
    signature: Option<u64>,
    /// The last pointer position, in the window's own logical pixels.
    cursor: Vector2f,
    /// Set by the window's own events: the host drops it at the next sync.
    closed: bool,
}

impl ToolWindow {
    /// Open one window for `request`, centered over the editor window for a
    /// modal and offset from its corner for a tool window.
    fn open(
        event_loop: &ActiveEventLoop,
        gpu: &PopupGpu<'_>,
        parent: &Window,
        request: &EditorWindowRequest,
    ) -> Result<ToolWindow, String> {
        let scale_factor = parent.scale_factor().max(0.01);
        let size = (
            (request.width as f64 * scale_factor).round().max(1.0) as u32,
            (request.height as f64 * scale_factor).round().max(1.0) as u32,
        );
        let origin = window_origin(parent, request, size, scale_factor);
        let attributes = Window::default_attributes()
            .with_title(request.title.as_str())
            .with_inner_size(winit::dpi::PhysicalSize::new(size.0, size.1))
            .with_position(winit::dpi::PhysicalPosition::new(origin.0, origin.1))
            .with_resizable(false);
        let window = Arc::new(
            event_loop
                .create_window(attributes)
                .map_err(|error| format!("the window was refused: {error}"))?,
        );
        let surface = gpu
            .instance
            .create_surface(Arc::clone(&window))
            .map_err(|error| format!("the window surface was refused: {error}"))?;
        let capabilities = surface.get_capabilities(gpu.adapter);
        if !capabilities.formats.contains(&gpu.format) {
            return Err(format!(
                "the window surface does not offer the editor's format {:?}",
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
            // The window's surface only offers opaque composition; the
            // shell's draw list paints a full background fill.
            alpha_mode: capabilities.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(gpu.device, &configuration);
        Ok(ToolWindow {
            id: request.id.clone(),
            window,
            surface,
            configuration,
            draw: request.draw.clone(),
            signature: None,
            cursor: Vector2f::ZERO,
            closed: false,
        })
    }

    /// Reconfigure the swapchain for the window's current size.
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

    /// Draw this window's list and present it, unless nothing changed.
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
        // The window may have been resized under us - a monitor change
        // (D30): the swapchain follows the window before it is drawn into.
        let size = self.window.inner_size();
        self.reconfigure(device, (size.width, size.height));
        let frame = self
            .surface
            .get_current_texture()
            .map_err(|error| format!("the window swapchain is unavailable: {error}"))?;
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("pill_editor window"),
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
// Placement
// =============================================================================

/// Where a window's top-left goes, in physical screen pixels: a modal
/// centers over the editor window, a tool window opens a small step from
/// its corner.
fn window_origin(
    parent: &Window,
    request: &EditorWindowRequest,
    size: (u32, u32),
    scale_factor: f64,
) -> (i32, i32) {
    let origin = parent
        .inner_position()
        .map(|position| (position.x, position.y))
        .unwrap_or((0, 0));
    match request.kind {
        pill_editor_api::EditorWindowKind::Modal => {
            let parent_size = parent.inner_size();
            (
                origin.0 + (parent_size.width as i32 - size.0 as i32) / 2,
                origin.1 + (parent_size.height as i32 - size.1 as i32) / 2,
            )
        }
        pill_editor_api::EditorWindowKind::Tool => {
            let offset = (TOOL_WINDOW_OFFSET * scale_factor).round() as i32;
            (origin.0 + offset, origin.1 + offset)
        }
    }
}

/// A hash of what a window would draw, for the redraw-on-change gate (D25).
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
