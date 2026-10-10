//! The contract between the editor executable and the reloadable shell
//! module.
//!
//! # Responsibilities
//!
//! - Defines the world resources the two sides exchange: [`EditorShellInput`]
//!   (what the executable feeds the shell each frame) and
//!   [`EditorShellFrame`] (what the shell hands back).
//! - Defines [`EditorRect`], the logical-pixel rectangle the scene surface
//!   is placed at.
//!
//! # Design
//!
//! Both sides link this crate as a shared `dylib`, so a resource of a type
//! defined here has one identity across the whole process - the same reason
//! `pill_ui` is shared. Everything crossing is plain data: no handles, no
//! device objects, no callbacks. The executable owns the window, the GPU
//! device and input; the shell owns panels, docking and its UI state, and
//! sees both only as data.
//!
//! The shell module reloads while the editor runs (D15). These resources are
//! re-claimed by every generation's registration, so the arriving generation
//! finds the values its predecessor built (the resource-claim contract
//! `pill_ui::UiContext` documents).

// External crates
use pill_core::math::Vector2f;
use pill_engine_core::{InputEvent, Resource};
use pill_ui::panel::ClipboardRequest;
use pill_ui::{AtlasUpdate, DrawList};

// =============================================================================
// EditorRect
// =============================================================================

/// A logical-pixel rectangle in the editor window's client area.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EditorRect {
    /// Distance from the client area's left edge.
    pub x: f32,
    /// Distance from the client area's top edge.
    pub y: f32,
    /// Extent along x.
    pub width: f32,
    /// Extent along y.
    pub height: f32,
}

impl EditorRect {
    /// A rectangle from its origin and size.
    pub fn new(x: f32, y: f32, width: f32, height: f32) -> EditorRect {
        EditorRect {
            x,
            y,
            width,
            height,
        }
    }

    /// The rectangle's size.
    pub fn size(&self) -> Vector2f {
        Vector2f::new(self.width, self.height)
    }
}

// =============================================================================
// EditorShellInput
// =============================================================================

/// What the executable feeds the shell each frame.
///
/// The executable queues input between frames and rewrites all of this
/// before every host frame; the shell's system drains `events` and reads the
/// rest.
#[derive(Clone, Debug, Default)]
pub struct EditorShellInput {
    /// This frame's input events, in logical pixels, in arrival order.
    pub events: Vec<InputEvent>,
    /// The client area's logical size.
    pub logical_size: Vector2f,
    /// The window's monitor scale factor.
    pub scale_factor: f32,
    /// The editor UI's own frame rate, for the Statistics panel.
    pub editor_frames_per_second: f32,
    /// A tab the executable asks the shell to select, by title; `None` when
    /// nothing was asked. The test channel drives focus between the Scene
    /// and Game views through this; nothing else in the editor writes it.
    pub requested_tab: Option<String>,
}

impl Resource for EditorShellInput {}

// =============================================================================
// EditorShellFrame
// =============================================================================

/// What the shell hands back each frame: everything the executable needs to
/// upload, draw and route.
#[derive(Clone, Debug, Default)]
pub struct EditorShellFrame {
    /// The UI under the scene layer: panels, tabs, docking chrome.
    pub panels: DrawList,
    /// The UI above the scene layer: badges, menus, drop previews, toasts.
    /// Drawn through the composition overlay when one exists, through the
    /// project renderer's UI pass otherwise (D24).
    pub overlay: DrawList,
    /// This frame's atlas update, uploaded once by the executable's
    /// renderer.
    pub atlas: Option<AtlasUpdate>,
    /// Clipboard operations the widgets asked for (D28), fulfilled by the
    /// executable's platform layer.
    pub clipboard: Vec<ClipboardRequest>,
    /// Where the scene surface goes, in logical pixels of the client area;
    /// `None` while no Scene tab is selected.
    pub scene_rect: Option<EditorRect>,
    /// Where the game surface goes, in logical pixels of the client area;
    /// `None` while no Game tab is selected (D29).
    pub game_rect: Option<EditorRect>,
}

impl EditorShellFrame {
    /// Forget the previous frame's output, keeping the allocations.
    pub fn clear(&mut self) {
        self.panels.clear();
        self.overlay.clear();
        self.atlas = None;
        self.clipboard.clear();
        self.scene_rect = None;
        self.game_rect = None;
    }
}

impl Resource for EditorShellFrame {}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clear_empties_every_output() {
        let mut frame = EditorShellFrame {
            atlas: None,
            scene_rect: Some(EditorRect::new(1.0, 2.0, 3.0, 4.0)),
            game_rect: Some(EditorRect::new(5.0, 6.0, 7.0, 8.0)),
            ..Default::default()
        };
        frame.panels.push_fill(
            pill_ui::Rect::from_min_size(Vector2f::ZERO, Vector2f::new(4.0, 4.0)),
            pill_ui::Color::WHITE,
        );
        frame.clipboard.push(ClipboardRequest::GetText);
        frame.clear();
        assert!(frame.panels.is_empty());
        assert!(frame.overlay.is_empty());
        assert!(frame.clipboard.is_empty());
        assert!(frame.scene_rect.is_none());
        assert!(frame.game_rect.is_none());
        assert!(frame.atlas.is_none());
    }
}
