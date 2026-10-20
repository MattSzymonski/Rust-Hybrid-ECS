//! The scene and game viewports: their geometry and overlay chips.
//!
//! # Responsibilities
//!
//! - The measured scene body rectangle the frontend places the scene's child window at, and the game view's own rectangle.
//! - The per-viewport overlay chips (the frame counter and the viewport buttons) and the regions they mark.
//!
//! Bodies unchanged from the shell's root file (W-0037 Stage 3); the globs
//! below are absolute paths, so a patch crate resolves them too.

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;

/// How far the scene rectangle extends past the measured tab body, in
/// logical points: the body's padding and outline sit inside the leaf, and
/// the child window must back them too, so no sliver of the game view
/// behind the overlay shows around the viewport's edge.
pub const SCENE_RECT_OVERSHOOT: f32 = 16.0;

/// A shell update slower than this logs its own `[editor] egui frame` line
/// in addition to the 210-frame heartbeat: the D32 bar for the whole UI pass
/// is 2 ms, so anything near it is worth a sample on a slow (debug) run.
pub const SLOW_UPDATE_MICROS: u64 = 800;

/// The scene's frame counter: its box in logical points, and the gap to the
/// tab body's corner.
pub const SCENE_COUNTER_WIDTH: f32 = 76.0;

pub const SCENE_COUNTER_HEIGHT: f32 = 20.0;

pub const SCENE_COUNTER_MARGIN: f32 = 8.0;

/// The example button beside the counter: its width, and the gap between
/// the two.
pub const SCENE_BUTTON_WIDTH: f32 = 72.0;

pub const SCENE_BUTTON_GAP: f32 = 6.0;

/// Convert an egui rectangle into the boundary's plain rectangle.
pub fn shell_rect(rect: egui::Rect) -> ShellRect {
    ShellRect {
        x: rect.min.x,
        y: rect.min.y,
        width: rect.width(),
        height: rect.height(),
    }
}

/// A scene tab's body rectangle in logical points, overshooting by
/// [`SCENE_RECT_OVERSHOOT`] so the child window backs the tab body's padding
/// and outline too.
pub fn scene_body_rect(ui: &egui::Ui) -> ShellRect {
    shell_rect(ui.max_rect().expand(SCENE_RECT_OVERSHOOT))
}

/// A scene tab's overlay strip, anchored to the viewport's top-right
/// corner: an example button and the frame counter, both drawn as the
/// theme's chips. The overlay paints above the scene's child window, so
/// both sit over the viewport, and both take input, so a click on either
/// never reaches the project. Both rectangles are recorded as the UI's
/// own, so a click on them is never read as a scene pick.
pub fn scene_overlay(
    ui: &mut egui::Ui,
    frames_per_second: f32,
    overlay_regions: &mut Vec<ShellRect>,
) {
    let counter_rect = frame_counter_rect(ui);
    let button_rect = egui::Rect::from_min_size(
        egui::pos2(
            counter_rect.left() - SCENE_BUTTON_WIDTH - SCENE_BUTTON_GAP,
            counter_rect.top(),
        ),
        egui::vec2(SCENE_BUTTON_WIDTH, SCENE_COUNTER_HEIGHT),
    );
    overlay_regions.push(shell_rect(counter_rect));
    overlay_regions.push(shell_rect(button_rect));
    let button = ui.put(
        button_rect,
        egui::Button::new("Example")
            .min_size(button_rect.size())
            .fill(style::CONTROL_FILL)
            .corner_radius(egui::CornerRadius::same(4)),
    );
    if button.clicked() {
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ENGINE,
            "[editor] example button clicked"
        );
    }
    // The counter is painted rather than laid out, so it costs the tab body
    // no layout space; its rectangle still senses clicks.
    let _ = ui.interact(
        counter_rect,
        ui.id().with("scene_frame_counter"),
        egui::Sense::click(),
    );
    ui.painter().rect(
        counter_rect,
        egui::CornerRadius::same(5),
        style::OVERLAY_FILL,
        egui::Stroke::new(1.0, style::OVERLAY_BORDER),
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        counter_rect.center(),
        egui::Align2::CENTER_CENTER,
        format!("{frames_per_second:.0} fps"),
        egui::FontId::monospace(12.0),
        style::TEXT_STRONG,
    );
}

/// The frame counter's rectangle: the strip's right-anchored box.
pub fn frame_counter_rect(ui: &egui::Ui) -> egui::Rect {
    egui::Rect::from_min_size(
        egui::pos2(
            ui.max_rect().right() - SCENE_COUNTER_WIDTH - SCENE_COUNTER_MARGIN,
            ui.max_rect().top() + SCENE_COUNTER_MARGIN,
        ),
        egui::vec2(SCENE_COUNTER_WIDTH, SCENE_COUNTER_HEIGHT),
    )
}
