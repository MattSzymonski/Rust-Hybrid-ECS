//! The plain data the egui frontend and the reloadable egui shell exchange.
//!
//! # Responsibilities
//!
//! - [`EguiShellInput`]: what the frontend writes for the shell each frame
//!   (surface geometry, scale, focus, time).
//! - [`EguiShellFrame`]: what the shell hands back (tessellated meshes,
//!   texture deltas, interaction state, cursor, clipboard), drained by the
//!   frontend.
//! - [`frame_from_output`]: builds the frame from egui's own output,
//!   dropping paint callbacks (plain data only crosses the boundary).
//!
//! # Design
//!
//! Everything crossing is plain data: no handles, no device objects, no
//! callbacks - the rule `pill_editor_api` states for the `pill_ui` editor,
//! kept for this one. Input events transit through the engine's `Input`
//! resource and the shell converts them itself ([`crate::input`]), so only
//! geometry and focus cross here.

// External crates
use egui::epaint::ClippedPrimitive;
use egui::{Context, CursorIcon, FullOutput, OutputCommand, TexturesDelta};
use pill_core::math::Vector2f;
use pill_engine_core::Resource;

/// The geometry and focus the frontend feeds the shell each frame.
#[derive(Clone, Copy, Debug)]
pub struct EguiShellInput {
    /// Logical size (points) of the overlay's client area.
    pub screen_size: Vector2f,
    /// Physical pixels per logical point.
    pub scale_factor: f32,
    /// Whether the editor window has keyboard focus.
    pub focused: bool,
    /// Seconds since the editor started, monotonic.
    pub time: f64,
}

impl Resource for EguiShellInput {}

impl Default for EguiShellInput {
    fn default() -> Self {
        Self {
            screen_size: Vector2f::new(1.0, 1.0),
            scale_factor: 1.0,
            focused: true,
            time: 0.0,
        }
    }
}

/// A logical-pixel rectangle crossing the boundary (IME placement).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ShellRect {
    /// Distance from the client area's left edge.
    pub x: f32,
    /// Distance from the client area's top edge.
    pub y: f32,
    /// Extent along x.
    pub width: f32,
    /// Extent along y.
    pub height: f32,
}

/// One frame of egui output, as plain data.
pub struct EguiShellFrame {
    /// Tessellated meshes to draw; paint callbacks are removed when the
    /// frame is built.
    pub meshes: Vec<ClippedPrimitive>,
    /// Texture uploads the renderer must apply before drawing.
    pub textures: TexturesDelta,
    /// The pixels-per-point the meshes were tessellated with.
    pub pixels_per_point: f32,
    /// egui wants the pointer: route clicks to the UI, not the viewports.
    pub wants_pointer: bool,
    /// egui wants the keyboard: route keys to the UI.
    pub wants_keyboard: bool,
    /// The cursor icon egui asked for.
    pub cursor_icon: CursorIcon,
    /// Text egui asked to copy to the clipboard.
    pub clipboard_text: Option<String>,
    /// IME composition should be enabled.
    pub ime_allowed: bool,
    /// Where the IME candidate window should appear, logical points.
    pub ime_rect: Option<ShellRect>,
}

impl Resource for EguiShellFrame {}

impl Default for EguiShellFrame {
    fn default() -> Self {
        Self {
            meshes: Vec::new(),
            textures: TexturesDelta::default(),
            pixels_per_point: 1.0,
            wants_pointer: false,
            wants_keyboard: false,
            cursor_icon: CursorIcon::Default,
            clipboard_text: None,
            ime_allowed: false,
            ime_rect: None,
        }
    }
}

/// Build the frontend's frame from egui's output for one pass.
///
/// Paint callbacks are tessellated into `ClippedPrimitive`s whose payload
/// is arbitrary user data; they are dropped here so only meshes cross.
pub fn frame_from_output(context: &Context, output: FullOutput) -> EguiShellFrame {
    let pixels_per_point = output.pixels_per_point;
    let meshes = context
        .tessellate(output.shapes, pixels_per_point)
        .into_iter()
        .filter(|clipped| matches!(clipped.primitive, egui::epaint::Primitive::Mesh(_)))
        .collect();
    let wants_pointer = context.wants_pointer_input();
    let wants_keyboard = context.wants_keyboard_input();
    let platform = output.platform_output;
    let mut clipboard_text = None;
    for command in &platform.commands {
        if let OutputCommand::CopyText(text) = command {
            clipboard_text = Some(text.clone());
        }
    }
    EguiShellFrame {
        meshes,
        textures: output.textures_delta,
        pixels_per_point,
        wants_pointer,
        wants_keyboard,
        cursor_icon: platform.cursor_icon,
        clipboard_text,
        // The engine reports IME composition in its input events; the
        // candidate window follows the text field egui focuses, which the
        // frontend reads off the frame's focus state for now.
        ime_allowed: wants_keyboard,
        ime_rect: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::RawInput;

    #[test]
    fn a_frame_reports_meshes_and_drops_callbacks() {
        let context = Context::default();
        let output = context.run(RawInput::default(), |context| {
            egui::CentralPanel::default().show(context, |ui| {
                ui.label("hello");
            });
        });
        let frame = frame_from_output(&context, output);
        assert!(!frame.meshes.is_empty());
        assert!(frame
            .meshes
            .iter()
            .all(|clipped| matches!(clipped.primitive, egui::epaint::Primitive::Mesh(_))));
        assert!(frame.pixels_per_point > 0.0);
    }

    #[test]
    fn a_frame_exposes_clipboard_commands() {
        let context = Context::default();
        let output = context.run(RawInput::default(), |context| {
            context.copy_text("copied".to_owned());
        });
        let frame = frame_from_output(&context, output);
        assert_eq!(frame.clipboard_text.as_deref(), Some("copied"));
    }
}
