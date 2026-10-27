//! The plain data the egui frontend and the reloadable egui shell exchange.
//!
//! # Responsibilities
//!
//! - [`EguiShellInput`]: what the frontend writes for the shell each frame
//!   (surface geometry, scale, focus, time, frame rate).
//! - [`EguiShellFrame`]: what the shell hands back (tessellated meshes,
//!   texture deltas, interaction state, cursor, clipboard), drained by the
//!   frontend.
//! - [`frame_from_output`]: builds the frame from egui's own output,
//!   dropping paint callbacks (plain data only crosses the boundary).
//!
//! # Design
//!
//! Everything crossing is plain data: no handles, no device objects, no
//! callbacks. The shell's input events travel in this resource as owned
//! [`InputEvent`]s, so the UI reads the events of its own windows and never
//! the game's.
// External crates
use egui::epaint::ClippedPrimitive;
use egui::{Context, CursorIcon, FullOutput, OutputCommand, TexturesDelta};
use pill_core::math::Vector2f;
use pill_engine::input::InputEvent;
use pill_engine::Resource;
use std::time::Duration;

/// The geometry and focus the frontend feeds the shell each frame.
#[derive(Clone, Debug)]
pub struct EguiShellInput {
    /// Logical size (points) of the overlay's client area.
    pub screen_size: Vector2f,
    /// Physical pixels per logical point.
    pub scale_factor: f32,
    /// Whether the editor window has keyboard focus.
    pub focused: bool,
    /// Seconds since the editor started, monotonic.
    pub time: f64,
    /// The editor's smoothed frame rate, for the shell's overlays.
    pub frames_per_second: f32,
    /// The events the editor's own windows reported since the last frame,
    /// in the main window's client coordinates.
    ///
    /// The game's window is not in here: its events go to the engine's
    /// `Input`, which is the whole point of the split.
    pub events: Vec<InputEvent>,
    /// The pointer's last known position in the same coordinates, when it is
    /// over the editor's windows at all.
    ///
    /// egui's button events name their own position, and a frame can carry a
    /// release with no move before it: this seeds the bridge with the place
    /// the pointer was last seen.
    pub cursor: Option<Vector2f>,
    /// The OS clipboard's text, carried when the frame's events include a paste
    /// chord.
    ///
    /// egui pastes text only from an [`egui::Event::Paste`], and only the
    /// frontend can read the OS clipboard, so it reads it once per paste chord
    /// and the bridge appends the event: without this a focused text field
    /// would ignore Ctrl+V.
    pub paste_text: Option<String>,
    /// The entities the executable projected into this frame's scene views,
    /// one entry per entity and view.
    ///
    /// The executable owns the views' cameras and the renderer's projection,
    /// so it is the side that can say where an entity lands on screen; the
    /// shell owns the icon atlas, so it is the side that draws the badge.
    pub badges: Vec<ShellBadge>,
    /// Whether a scripted channel is driving this run.
    ///
    /// The row rectangles the Inspector logs exist for a suite - it drops an
    /// asset onto one without reading pixels - and a person reading the console
    /// gains nothing from a line per row placement. Only a scripted run asks
    /// for them.
    pub scripted: bool,
}

/// One entity's badge in one scene view.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShellBadge {
    /// Which scene view the badge belongs to.
    pub view: usize,
    /// Where the entity projects to, in the main window's logical points.
    pub x: f32,
    /// Where the entity projects to, in the main window's logical points.
    pub y: f32,
    /// The entity the badge names; a click selects it.
    pub entity: pill_engine::Entity,
}

/// How many badges one scene view draws: enough for a scene's population to
/// read at a glance, few enough that the overlay never turns into a wall of
/// chips.
pub const BADGES_PER_VIEW_MAX: usize = 48;

impl Resource for EguiShellInput {}

impl Default for EguiShellInput {
    fn default() -> Self {
        Self {
            screen_size: Vector2f::new(1.0, 1.0),
            scale_factor: 1.0,
            focused: true,
            time: 0.0,
            frames_per_second: 0.0,
            events: Vec::new(),
            cursor: None,
            paste_text: None,
            badges: Vec::new(),
            scripted: false,
        }
    }
}

/// One input record per popped-out tab's window.
///
/// A popped-out window's events must not reach the main window's pass - its
/// coordinates are its own client area's - so the frontend keeps them apart
/// and the shell reads the tab's record when it runs that tab's pass. A tab
/// with no record (a window the frontend has not made yet) gets a default.
#[derive(Clone, Debug, Default)]
pub struct EguiPopoutInputs {
    /// Each popped-out tab's input, keyed by the tab's wire name.
    pub by_tab: std::collections::BTreeMap<String, EguiShellInput>,
}

impl Resource for EguiPopoutInputs {}

/// How many scene viewports a layout opens with: the two the default dock
/// carries. The shell may report more - a view the person added - and the
/// frontend opens one for each it does.
pub const DEFAULT_SCENE_VIEWS: usize = 2;

/// The most scene views one editor opens: each view is a child window and a
/// renderer surface, so a layout that named thousands is trimmed to this.
pub const SCENE_VIEWS_MAX: usize = 8;

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

impl ShellRect {
    /// Whether a logical point lies inside the rectangle, edges included.
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x <= self.x + self.width && y >= self.y && y <= self.y + self.height
    }
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
    /// The scene viewports' body rectangles (logical points), one slot per
    /// view index: the frontend places that view's child window here, so
    /// each viewport follows the dock layout. A slot the shell leaves `None`
    /// hides that viewport's window - its tab is not shown - and the list is
    /// only as long as the highest view index the dock holds.
    pub scene_rects: Vec<Option<ShellRect>>,
    /// The game viewport's body rectangle (logical points): the frontend
    /// places the primary surface's child window here, so the game view
    /// follows the dock layout. `None` hides that window - the Game tab is
    /// not shown.
    pub game_rect: Option<ShellRect>,
    /// The overlay's opaque chips this pass (logical points): a click
    /// inside one over a scene viewport is the UI's, not the scene's, so
    /// the frontend routes it without asking the scene what is under it.
    pub overlay_regions: Vec<ShellRect>,
    /// IME composition should be enabled.
    pub ime_allowed: bool,
    /// Where the IME candidate window should appear, logical points.
    pub ime_rect: Option<ShellRect>,
    /// The asset path a tile drag handed to the OS: the
    /// frontend starts an OLE drag with it, so a file can be dragged into
    /// Explorer.
    pub drag_out: Option<String>,
    /// The shell's menu asked the editor to quit. The winit frontend honours
    /// it through the same path a window close takes; a browser frontend
    /// ignores it (a page cannot close itself safely).
    pub quit_requested: bool,
    /// When egui asked for the next pass: `Some(Duration::ZERO)` draws again
    /// at once, `None` means no pass was requested. The frontend's redraw
    /// cadence honours it.
    pub repaint_after: Option<Duration>,
    /// One frame per tab popped out into its own OS window:
    /// the shell runs a pass of its own context for each, and the frontend
    /// paints it into that tab's window. A tab that appears here has its
    /// window created; one that disappears is closed.
    pub popout_frames: Vec<PopoutFrame>,
}

/// One popped-out tab's frame: which tab it draws, and the pass's output.
pub struct PopoutFrame {
    /// The tab's wire name, which is also its window's identity.
    pub tab: String,
    /// The window's title, as the shell wants it shown.
    pub title: String,
    /// The pass's output, in the same shape the main window's frame takes.
    pub frame: Box<EguiShellFrame>,
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
            scene_rects: Vec::new(),
            game_rect: None,
            overlay_regions: Vec::new(),
            ime_allowed: false,
            ime_rect: None,
            drag_out: None,
            quit_requested: false,
            repaint_after: None,
            popout_frames: Vec::new(),
        }
    }
}

/// Build the frontend's frame from egui's output for one pass.
///
/// Paint callbacks are tessellated into `ClippedPrimitive`s whose payload
/// is arbitrary user data; they are dropped here so only meshes cross.
pub fn frame_from_output(context: &Context, output: FullOutput) -> EguiShellFrame {
    let pixels_per_point = output.pixels_per_point;
    // The earliest deadline any viewport named; `Duration::MAX` is egui's
    // "no pass requested".
    let repaint_after = output
        .viewport_output
        .values()
        .map(|viewport| viewport.repaint_delay)
        .filter(|delay| *delay != Duration::MAX)
        .min();
    let meshes = context
        .tessellate(output.shapes, pixels_per_point)
        .into_iter()
        .filter(|clipped| matches!(clipped.primitive, egui::epaint::Primitive::Mesh(_)))
        .collect();
    let wants_pointer = context.egui_wants_pointer_input();
    let wants_keyboard = context.egui_wants_keyboard_input();
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
        // The shell fills the scene rectangles after its dock pass runs;
        // `frame_from_output` only knows egui's own output, and the list's
        // length is the number of views the dock holds.
        scene_rects: Vec::new(),
        // Same for the game rectangle: the shell records it as it draws
        // the Game tab.
        game_rect: None,
        // Same for the overlay chips: the shell records them as it draws.
        overlay_regions: Vec::new(),
        // The engine reports IME composition in its input events; this frame
        // allows IME exactly while egui wants the keyboard, since the
        // candidate window follows the text field that has focus.
        ime_allowed: wants_keyboard,
        ime_rect: None,
        // Only the shell asks for a drag out; it records the path itself.
        drag_out: None,
        // Only the shell's menu can ask to quit; it sets the flag itself.
        quit_requested: false,
        repaint_after,
        // A popped-out tab's pass is its own; this frame is the main window's.
        popout_frames: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::RawInput;

    #[test]
    fn a_frame_reports_meshes_and_drops_callbacks() {
        let context = Context::default();
        let output = context.run_ui(RawInput::default(), |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.label("hello");
            });
        });
        let mut frame = frame_from_output(&context, output);
        assert!(!frame.meshes.is_empty());
        assert!(frame
            .meshes
            .iter()
            .all(|clipped| matches!(clipped.primitive, egui::epaint::Primitive::Mesh(_))));
        assert!(frame.pixels_per_point > 0.0);
        assert!(frame.overlay_regions.is_empty());
        // No GPU here: epaint panics on an unapplied delta, so drop it the
        // documented way instead of with the frame.
        frame.textures.clear();
    }

    #[test]
    fn a_shell_rect_contains_its_edges() {
        let rect = ShellRect {
            x: 10.0,
            y: 20.0,
            width: 100.0,
            height: 50.0,
        };
        assert!(rect.contains(10.0, 20.0));
        assert!(rect.contains(110.0, 70.0));
        assert!(rect.contains(60.0, 45.0));
        assert!(!rect.contains(9.9, 45.0));
        assert!(!rect.contains(60.0, 70.1));
    }

    #[test]
    fn a_frame_exposes_clipboard_commands() {
        let context = Context::default();
        let output = context.run_ui(RawInput::default(), |ui| {
            ui.ctx().copy_text("copied".to_owned());
        });
        let mut frame = frame_from_output(&context, output);
        assert_eq!(frame.clipboard_text.as_deref(), Some("copied"));
        frame.textures.clear();
    }
}
