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

/// Panel registration: the registry, the context and the liveness rule.
pub mod panels;

/// The editor's console ring: the bounded capture buffer lines land in.
///
/// A thin newtype so the resource lives at the editor boundary; the buffer
/// itself is `pill_core`'s (a shared dylib), so its identity crosses the
/// executable, the shell and every module that logs into it.
pub struct EditorLogs(pub pill_core::telemetry::capture::LogBuffer);

impl Resource for EditorLogs {}

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
    /// A module panel the executable asks the shell to focus or open (the
    /// scripted test channel; the same path a restored layout takes for a
    /// panel whose module is not loaded).
    pub requested_panel: Option<String>,
    /// A widget the executable asks the shell to activate, by name: the
    /// shell synthesizes a click on the named widget's rectangle (the
    /// scripted test channel's addressable activation).
    pub requested_widget: Option<String>,
    /// Lines to wheel-scroll at the last activated widget, positive up
    /// (the scripted test channel).
    pub requested_wheel: Option<f32>,
    /// Command ids the executable asks the shell to execute (the scripted
    /// test channel); drained through the same dispatch a shortcut uses.
    pub requested_commands: Vec<String>,
    /// Files the OS drag currently hovers over the window (Stage 18); the
    /// shell previews where they would land.
    pub hovered_files: Vec<String>,
    /// Files the OS dropped this frame; the shell queues the import.
    pub dropped_files: Vec<String>,
    /// The project's asset root (absolute), for building the import's
    /// destination; `None` when the project has none.
    pub asset_root: Option<String>,
    /// Whether the engine's systems are paused, published by the executable
    /// because only it holds the `Engine`; the pause command toggles from it.
    pub systems_paused: bool,
    /// The problems the host's notices produced, and fresh toast events
    /// (Stage 19). The executable rewrites `problems` and `revision` and
    /// appends to `events`; the shell drains `events` and reacts to a
    /// revision change.
    pub notices: EditorNotices,
    /// Where the pointer is over each open popup (Stage 21), in the popup's
    /// own logical coordinates, and whether the left button came up there
    /// this frame. A popup is a window of its own, so its pointer never
    /// arrives as an [`InputEvent`]; this is that channel.
    pub popup_pointers: Vec<EditorPopupPointer>,
    /// Popups the platform closed this frame (Stage 21): a click outside, a
    /// lost focus or an Escape the operating system took. The shell clears
    /// the matching state, exactly as it would have on its own dismissal.
    pub popup_closed: Vec<String>,
    /// The pointers and button edges the free-standing windows collected
    /// since the shell's last frame (Stage 24), in the window's own logical
    /// pixels: a tool window's content is a panel, and its widgets answer
    /// the same input a docked panel's do, only through this channel.
    pub window_pointers: Vec<EditorWindowPointer>,
    /// Enter and Escape presses from a window's own keyboard stream
    /// (Stage 24); a modal answers them, anything else ignores them.
    pub window_keys: Vec<EditorWindowKey>,
    /// Committed text from a window's own keyboard stream (Stage 25): the
    /// typing a panel's field inside a tool window or dialog receives. A
    /// window's keys beyond Enter and Escape stay on its side of the
    /// channel until a panel needs them.
    pub window_text: Vec<EditorWindowText>,
    /// Windows the platform closed this frame (Stage 24): the user pressed
    /// the close button. The shell drops the window's state, exactly as it
    /// would have on its own close.
    pub window_closed: Vec<String>,
    /// Every open window's arrangement, as the platform reports it (Stage
    /// 29): where it sits and how large it is. The shell folds it into the
    /// layout (a popped-out tab's size and position) and the recovery
    /// document (every window), which is what a restart reopens from.
    pub window_bounds: Vec<EditorWindowBounds>,
    /// Screen-space labels the overlay draws over the scene (Stage 21), in
    /// logical client pixels. The executable publishes them because it is
    /// the side that projects world positions; the shell only paints them.
    pub overlay_labels: Vec<EditorOverlayLabel>,
}

impl Resource for EditorShellInput {}

/// One label the overlay draws over the scene (Stage 21).
///
/// The executable projects a world position to the client area and hands the
/// text over; the shell draws it in the overlay layer, where it can sit on
/// top of the scene view's own window.
#[derive(Clone, Debug, PartialEq)]
pub struct EditorOverlayLabel {
    /// The text to draw.
    pub text: String,
    /// Where the text's top-left goes, in logical client pixels.
    pub position: Vector2f,
}

/// One popup's pointer this frame (Stage 21).
///
/// Positions are in the popup's own logical pixels, so the shell hit-tests
/// the rectangles it painted for that popup without knowing where the
/// platform put the window - it may have flipped or slid it at the monitor's
/// work area.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EditorPopupPointer {
    /// Which popup, by [`EditorPopup::id`].
    pub id: String,
    /// The pointer's position in the popup's own logical pixels.
    pub position: Vector2f,
    /// Whether the left button came up over the popup this frame.
    pub released: bool,
}

/// One popup the shell asks the platform to open (Stage 21, D23).
///
/// A popup is an owned, undecorated top-level window of its own, so its
/// content may extend past the editor window; the shell names where it wants
/// it relative to the client area and what to draw in it. Nothing above the
/// platform layer assumes the popup is a `winit` window (the Linux half,
/// Stage 31, opens an `xdg_popup` for the same request).
#[derive(Clone, Debug, Default)]
pub struct EditorPopup {
    /// The popup's identity, stable while it stays open; the shell and the
    /// executable name it in [`EditorPopupPointer`] and
    /// [`EditorShellInput::popup_closed`].
    pub id: String,
    /// Where the popup's top-left goes, in logical pixels of the client
    /// area: the anchor the platform places and flips at the work area.
    pub rect: EditorRect,
    /// What to draw, in the popup's own logical pixels (its top-left is the
    /// origin), premultiplied over whatever is behind it.
    pub draw: DrawList,
}

/// The kind of free-standing window the shell asks for (Stage 24).
///
/// A **tool window** is an ordinary decorated top-level window the user
/// closes; a **modal dialog** additionally blocks input to the rest of the
/// editor while it is open, answers Enter and Escape, and answers each of
/// its buttons with the button's command.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EditorWindowKind {
    /// A free-standing tool window, shown until closed.
    #[default]
    Tool,
    /// A modal dialog: it blocks the editor's input while it is open.
    Modal,
}

impl EditorWindowKind {
    /// The kind's name for logs.
    pub fn title(&self) -> &'static str {
        match self {
            EditorWindowKind::Tool => "tool",
            EditorWindowKind::Modal => "modal",
        }
    }
}

/// One button of a modal (Stage 24): the label drawn in the button row and
/// the command its activation runs. An empty command is a plain dismissal
/// (Cancel): the modal closes and nothing else happens.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EditorWindowButton {
    /// The button's label.
    pub label: String,
    /// The command its activation executes, empty for a dismissal.
    pub command: String,
}

/// One free-standing window the shell asks the executable for (Stage 24).
///
/// The request names a registered panel, never a function pointer or a
/// closure: the shell resolves it through the registry each frame, so the
/// window always draws the current generation's code and dies with it. A
/// window's kind, title, size and buttons are fixed while it is open; the
/// draw list is rebuilt every frame like a popup's.
#[derive(Clone, Debug, Default)]
pub struct EditorWindowRequest {
    /// The window's identity, stable while it stays open; the shell and the
    /// executable name it in [`EditorWindowPointer`], [`EditorWindowKey`]
    /// and [`EditorShellInput::window_closed`].
    pub id: String,
    /// Tool window or modal dialog.
    pub kind: EditorWindowKind,
    /// The window's title, drawn by the platform in its title bar.
    pub title: String,
    /// The window's width in logical pixels.
    pub width: f32,
    /// The window's height in logical pixels.
    pub height: f32,
    /// The registered panel the window shows.
    pub panel: String,
    /// The modal's buttons, in draw order, left to right; empty for a tool
    /// window. The first button is the modal's default: Enter answers it.
    pub buttons: Vec<EditorWindowButton>,
    /// Where the window's outer top-left sits on the desktop, in logical
    /// pixels, when a remembered arrangement is being restored (Stage 29);
    /// `None` lets the host place the window (a modal centers over its
    /// owner, a tool window opens near its corner).
    pub position: Option<(f32, f32)>,
    /// What to draw, in the window's own logical pixels (its top-left is
    /// the origin): the panel, and for a modal the button row.
    pub draw: DrawList,
}

/// One open window's arrangement, as the platform reports it (Stage 29):
/// the shell folds it back into the layout and the recovery document, so a
/// restart reopens the windows where the user left them.
#[derive(Clone, Debug, PartialEq)]
pub struct EditorWindowBounds {
    /// The window's id, as its request named it.
    pub id: String,
    /// The window's outer top-left on the desktop, in logical pixels; `None`
    /// where the platform reports no global position (Wayland, D23).
    pub position: Option<(f32, f32)>,
    /// The window's client width in logical pixels.
    pub width: f32,
    /// The window's client height in logical pixels.
    pub height: f32,
}

/// One pointer event of one free-standing window (Stage 24), in the
/// window's own logical pixels.
///
/// A tool window is a window of its own, so its pointer never arrives as an
/// [`InputEvent`]; this is that channel, and the press and release edges
/// come through it so the panel's widgets answer exactly as they do in a
/// dock.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EditorWindowPointer {
    /// Which window, by [`EditorWindowRequest::id`].
    pub id: String,
    /// The pointer's position in the window's own logical pixels.
    pub position: Vector2f,
    /// What happened: a move, the left button going down, or coming up.
    pub kind: EditorWindowPointerKind,
}

/// The kind of pointer event a window collected (Stage 24).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EditorWindowPointerKind {
    /// The pointer moved.
    #[default]
    Moved,
    /// The left button went down.
    Pressed,
    /// The left button came up.
    Released,
}

/// One key press of one free-standing window (Stage 24).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EditorWindowKey {
    /// Which window, by [`EditorWindowRequest::id`].
    pub id: String,
    /// Which key: the two a modal answers.
    pub key: EditorWindowKeyKind,
}

/// The keys a modal answers (Stage 24): Enter activates its default button,
/// Escape dismisses it without a command.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EditorWindowKeyKind {
    /// The default button's command runs.
    #[default]
    Enter,
    /// The modal closes, nothing runs.
    Escape,
}

/// One committed text run from one free-standing window (Stage 25): what a
/// field inside it receives when the user types there.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EditorWindowText {
    /// Which window, by [`EditorWindowRequest::id`].
    pub id: String,
    /// The text just committed.
    pub text: String,
}

/// One structured clipboard write the shell requests (Stage 18): the payload
/// bytes under `format`, offered to other applications alongside `text`.
#[derive(Clone, Debug, PartialEq)]
pub struct ClipboardObject {
    /// Registered clipboard format name (see
    /// `pill_editor_core::clipboard::FORMAT_NAME`).
    pub format: String,
    /// The payload document's bytes.
    pub bytes: Vec<u8>,
    /// The same payload as readable text (what a plain editor pastes).
    pub text: String,
}

/// The answer to a clipboard read, filled by the executable and consumed by
/// the shell on a later frame.
#[derive(Clone, Debug, Default)]
pub struct EditorClipboardAnswer {
    /// The format that was asked for.
    pub format: String,
    /// The payload bytes, when the owner offered the format.
    pub bytes: Option<Vec<u8>>,
    /// The plain text, when the owner offered any (the fallback source).
    pub text: Option<String>,
    /// The read this answer belongs to; the shell matches its pending
    /// request against it.
    pub seq: u64,
}

impl Resource for EditorClipboardAnswer {}

// =============================================================================
// EditorNotices
// =============================================================================

/// How serious one notice or problem entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditorProblemLevel {
    /// A build or a patch failed; the previous generation keeps running.
    Error,
    /// The compiler's warning.
    Warning,
    /// Anything else worth listing.
    Note,
}

/// One row of the Problems panel (Stage 19).
#[derive(Clone, Debug, PartialEq)]
pub struct EditorProblem {
    /// The module whose build, load or patch reported it.
    pub module: String,
    /// How serious it is.
    pub level: EditorProblemLevel,
    /// The message, the compiler's own words for a build failure.
    pub message: String,
    /// The source file the report named, workspace-relative as written.
    pub file: Option<String>,
    /// 1-based line inside `file`.
    pub line: Option<u32>,
    /// 1-based column inside `line`.
    pub column: Option<u32>,
}

/// One toast event (Stage 19): a short-lived line about something the host
/// did.
#[derive(Clone, Debug, PartialEq)]
pub struct EditorNoticeEvent {
    /// A per-session monotonic id; the shell's toast list keys on it.
    pub seq: u64,
    /// How serious it is, choosing the toast's color.
    pub level: EditorProblemLevel,
    /// The line to show.
    pub text: String,
}

/// What the executable publishes for the shell: the problems the host's
/// notices produced, and fresh toast events (Stage 19).
///
/// Carried inside [`EditorShellInput`] rather than as a resource of its own:
/// the shell's system already takes the input, and the executable already
/// rewrites it every frame.
#[derive(Clone, Debug, Default)]
pub struct EditorNotices {
    /// Bumped whenever `problems` or `events` changed; the shell reacts to a
    /// new value instead of diffing the lists.
    pub revision: u64,
    /// The current problems, unsorted: `module` names the owner.
    pub problems: Vec<EditorProblem>,
    /// Toast events not yet consumed by the shell.
    pub events: Vec<EditorNoticeEvent>,
    /// The id `events`' next entry takes.
    pub next_event_seq: u64,
}

/// A file the shell asks the executable to open in the developer's editor
/// (Stage 19): where a Problems row points.
#[derive(Clone, Debug, PartialEq)]
pub struct EditorOpenRequest {
    /// Project- or workspace-relative path, as the compiler named it.
    pub file: String,
    /// 1-based line to land on; 0 opens the file at its start.
    pub line: u32,
}

// =============================================================================
// EditorViewRect
// =============================================================================

/// Which camera a view surface draws through (Stage 29).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EditorViewKind {
    /// The editor's own camera (D29); several scene views may render at
    /// once, each owning its camera.
    #[default]
    Scene,
    /// The project's own camera; the selected Game view shows it.
    Game,
}

/// One view surface's placement for this frame (Stage 29).
///
/// The id names the dock tab that owns the view, so a view keeps its
/// surface, its camera and its placement across frames and restarts.
#[derive(Clone, Debug, PartialEq)]
pub struct EditorViewRect {
    /// Stable identity, from the tab's node (`view:7`).
    pub id: String,
    /// The tab's numeric node: the smallest scene node is the primary
    /// view, which keeps the boot-time surface id.
    pub node: u64,
    /// The camera the view draws through.
    pub kind: EditorViewKind,
    /// Where the view goes, in logical pixels of the window named below
    /// (or of the main window's client area).
    pub rect: EditorRect,
    /// The popped-out window that holds the view (Stage 29), by its window
    /// request id; `None` for a view docked in the main window.
    pub window: Option<String>,
}

// =============================================================================
// EditorShellFrame
// =============================================================================

/// What the shell hands back each frame: everything the executable needs to
/// upload, draw and route.
#[derive(Clone, Debug, Default)]
pub struct EditorShellFrame {
    /// Frames the shell has built since it loaded (Stage 22), stamped at the
    /// end of each update: the executable watches it advance to tell a live
    /// shell from a generation that stopped running, which is what its safe
    /// mode recovers from.
    pub frames: u64,
    /// The UI under the scene layer: panels, tabs, docking chrome.
    pub panels: DrawList,
    /// The UI above the scene layer: badges, menus, drop previews, toasts.
    /// Drawn through the composition overlay when one exists, through the
    /// project renderer's UI pass otherwise (D24).
    pub overlay: DrawList,
    /// The overlay's opaque regions, in logical pixels of the client area
    /// (Stage 21): input over one of them is the editor UI's, input beside
    /// them is the scene's, and everything the list leaves out lets the
    /// scene show through.
    pub overlay_opaque: Vec<EditorRect>,
    /// The popups the shell wants open (Stage 21, D23): menus, context menus
    /// and tooltips that may extend past the editor window. A popup the
    /// shell leaves out is closed.
    pub popups: Vec<EditorPopup>,
    /// The free-standing windows the shell wants open (Stage 24): tool
    /// windows and modal dialogs, with their content for this frame. A
    /// window the shell leaves out is closed; one it keeps is created at
    /// its requested size and drawn from `draw`. Which windows stay open -
    /// and which reopen after their module reloads - is the shell's own
    /// decision, published through this list.
    pub windows: Vec<EditorWindowRequest>,
    /// The open modal's id (Stage 24), while one is: the executable drops
    /// the editor's own input until it is answered or dismissed, so a modal
    /// dialog blocks the editor the way one should - and leaves the running
    /// game's input alone, which arrives through the view surfaces' own
    /// windows.
    pub modal: Option<String>,
    /// This frame's atlas update, uploaded once by the executable's
    /// renderer.
    pub atlas: Option<AtlasUpdate>,
    /// Clipboard operations the widgets asked for (D28), fulfilled by the
    /// executable's platform layer.
    pub clipboard: Vec<ClipboardRequest>,
    /// Structured payloads the shell's copy commands produced (D28); the
    /// executable writes each under its format plus its readable text.
    pub objects: Vec<ClipboardObject>,
    /// A clipboard payload the shell asks the executable to read, by
    /// registered format name (the paste command's side of D28); the answer
    /// lands in [`EditorClipboardAnswer`] on a later frame, so a slow
    /// clipboard owner never stalls the event loop.
    pub requested_clipboard: Option<String>,
    /// Where the Assets panel sits, in logical pixels, when it is visible;
    /// the executable aims a scripted file drop at it (Stage 18).
    pub assets_rect: Option<EditorRect>,
    /// An asset row held down this frame, named by its source path relative
    /// to the asset root; the executable turns a drag that leaves the window
    /// into an OS drag of that file (Stage 18).
    pub drag_candidate: Option<String>,
    /// A file the shell asks the executable to open in the developer's
    /// editor, from a Problems row (Stage 19).
    pub requested_open: Option<EditorOpenRequest>,
    /// The scene and game views to draw this frame (Stage 29): one entry
    /// per selected Scene or Game tab, each into its own surface. A view
    /// whose `window` names a popped-out tab's window draws inside that
    /// window's client area instead of the main window's.
    pub views: Vec<EditorViewRect>,
}

impl EditorShellFrame {
    /// Forget the previous frame's output, keeping the allocations.
    pub fn clear(&mut self) {
        self.panels.clear();
        self.overlay.clear();
        self.overlay_opaque.clear();
        self.popups.clear();
        self.windows.clear();
        self.modal = None;
        self.atlas = None;
        self.clipboard.clear();
        self.objects.clear();
        self.requested_clipboard = None;
        self.assets_rect = None;
        self.drag_candidate = None;
        self.requested_open = None;
        self.views.clear();
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
            views: vec![EditorViewRect {
                id: "view:1".to_string(),
                node: 1,
                kind: EditorViewKind::Scene,
                rect: EditorRect::new(1.0, 2.0, 3.0, 4.0),
                window: None,
            }],
            ..Default::default()
        };
        frame.panels.push_fill(
            pill_ui::Rect::from_min_size(Vector2f::ZERO, Vector2f::new(4.0, 4.0)),
            pill_ui::Color::WHITE,
        );
        frame.clipboard.push(ClipboardRequest::GetText);
        frame.objects.push(ClipboardObject {
            format: "PillEditor.Object".to_string(),
            bytes: vec![1, 2, 3],
            text: "{}".to_string(),
        });
        frame.requested_clipboard = Some("PillEditor.Object".to_string());
        frame.drag_candidate = Some("a.png".to_string());
        frame.requested_open = Some(EditorOpenRequest {
            file: "modules/x.rs".to_string(),
            line: 7,
        });
        frame
            .overlay_opaque
            .push(EditorRect::new(1.0, 2.0, 3.0, 4.0));
        frame.popups.push(EditorPopup {
            id: "menu.dropdown".to_string(),
            rect: EditorRect::new(1.0, 2.0, 30.0, 40.0),
            draw: DrawList::default(),
        });
        frame.clear();
        assert!(frame.panels.is_empty());
        assert!(frame.overlay.is_empty());
        assert!(frame.overlay_opaque.is_empty());
        assert!(frame.popups.is_empty());
        assert!(frame.clipboard.is_empty());
        assert!(frame.objects.is_empty());
        assert!(frame.requested_clipboard.is_none());
        assert!(frame.drag_candidate.is_none());
        assert!(frame.requested_open.is_none());
        assert!(frame.views.is_empty());
        assert!(frame.atlas.is_none());
    }
}
