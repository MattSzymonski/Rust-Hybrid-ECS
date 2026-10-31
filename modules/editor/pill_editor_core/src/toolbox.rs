//! The floating viewport toolbox: its rows, geometry and the
//! commands a click on a row queues.
//!
//! # Responsibilities
//!
//! - Name the toolbox's rows ([`ToolboxItem`]: the three gizmo tools, the
//!   space toggle, the snapping toggle and the scene grid's switch) and the
//!   order the panel stacks them in ([`TOOLBOX_ITEMS`]).
//! - The vocabulary the rows share with the rest of the editor: the label a
//!   row shows for the current state ([`ToolboxItem::label`]), its icon
//!   ([`ToolboxItem::icon`]), whether it reads as active
//!   ([`ToolboxItem::active`]), its name in the log, and the command a click
//!   queues ([`ToolboxItem::command`]).
//! - The panel's geometry: where it defaults to inside a viewport
//!   ([`default_origin`]), how a drag is clamped inside it
//!   ([`clamped_origin`]), and the rectangles of the panel, its drag handle
//!   and each row ([`panel_rect`], [`handle_rect`], [`item_rect`]).
//!
//! # Design
//!
//! Pure functions and plain constants over egui's geometry types: a click's
//! target, the region a press must not become a scene pick and the drag's
//! clamp all read the numbers the shell draws. A row queues the same
//! `SetGizmoTool` edit the shortcuts, the viewport menu and Preferences do.

// External crates
use egui::{Pos2, Rect, Vec2};

// Current crate
use crate::editor_state::EditorCommand;
use crate::gizmo::{GizmoMode, GizmoSettings, GizmoSpace};
use crate::icons::Icon;

// =============================================================================
// Geometry
// =============================================================================

/// The panel's width, in logical points: wide enough for a row's icon, its
/// label and the button padding.
pub const TOOLBOX_WIDTH: f32 = 124.0;

/// The gap between the panel and the viewport's edges: the default anchor
/// and the clamp both keep it, so a dragged toolbox never touches the edge.
pub const TOOLBOX_MARGIN: f32 = 10.0;

/// The drag handle's height: the panel's full-width top strip.
pub const TOOLBOX_HANDLE_HEIGHT: f32 = 16.0;

/// The gap between the handle and the first row, and between two rows.
pub const TOOLBOX_ITEM_GAP: f32 = 4.0;

/// One row's height: tall enough for the kit's chunky face (32 points) with a
/// little air above and below it, since the panel's rows wear that face.
pub const TOOLBOX_ITEM_HEIGHT: f32 = 44.0;
/// The inset between a row and the panel's sides, and below the last row.
pub const TOOLBOX_PADDING: f32 = 6.0;

/// The panel's size: the handle, then every row, with the bottom padding.
pub fn panel_size() -> Vec2 {
    let rows = TOOLBOX_ITEMS.len() as f32;
    Vec2::new(
        TOOLBOX_WIDTH,
        TOOLBOX_HANDLE_HEIGHT
            + TOOLBOX_ITEM_GAP
            + rows * TOOLBOX_ITEM_HEIGHT
            + (rows - 1.0) * TOOLBOX_ITEM_GAP
            + TOOLBOX_PADDING,
    )
}

/// The panel's rectangle for one top-left origin.
pub fn panel_rect(origin: Pos2) -> Rect {
    Rect::from_min_size(origin, panel_size())
}

/// The drag handle's rectangle: the panel's top strip, the one part of the
/// panel a press drags instead of clicking.
pub fn handle_rect(origin: Pos2) -> Rect {
    Rect::from_min_size(origin, Vec2::new(TOOLBOX_WIDTH, TOOLBOX_HANDLE_HEIGHT))
}

/// One row's rectangle inside the panel.
pub fn item_rect(item: ToolboxItem, origin: Pos2) -> Rect {
    let index = TOOLBOX_ITEMS
        .iter()
        .position(|candidate| *candidate == item)
        .unwrap_or(0) as f32;
    Rect::from_min_size(
        Pos2::new(
            origin.x + TOOLBOX_PADDING,
            origin.y
                + TOOLBOX_HANDLE_HEIGHT
                + TOOLBOX_ITEM_GAP
                + index * (TOOLBOX_ITEM_HEIGHT + TOOLBOX_ITEM_GAP),
        ),
        Vec2::new(TOOLBOX_WIDTH - 2.0 * TOOLBOX_PADDING, TOOLBOX_ITEM_HEIGHT),
    )
}

/// Where a toolbox parks before it is dragged: the viewport's top-left
/// corner, one margin in.
pub fn default_origin(viewport: Rect) -> Pos2 {
    Pos2::new(
        viewport.left() + TOOLBOX_MARGIN,
        viewport.top() + TOOLBOX_MARGIN,
    )
}

/// A drag's result: the requested origin pulled back inside the viewport.
///
/// A viewport smaller than the panel cannot hold it; the clamp then keeps
/// the panel's top-left corner on screen - the rest may overshoot, which
/// still leaves the handle and the first rows reachable.
pub fn clamped_origin(origin: Pos2, viewport: Rect) -> Pos2 {
    let size = panel_size();
    let minimum = default_origin(viewport);
    let maximum = Pos2::new(
        (viewport.right() - size.x - TOOLBOX_MARGIN).max(minimum.x),
        (viewport.bottom() - size.y - TOOLBOX_MARGIN).max(minimum.y),
    );
    Pos2::new(
        origin.x.clamp(minimum.x, maximum.x),
        origin.y.clamp(minimum.y, maximum.y),
    )
}

// =============================================================================
// Rows
// =============================================================================

/// The state the toolbox's rows read and write through their commands: the
/// gizmo's settings and the scene grid's switch.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ToolboxState {
    /// The gizmo's mode, space and snapping.
    pub gizmo: GizmoSettings,
    /// Whether the scene grid is on.
    pub grid: bool,
}

/// One row of the floating toolbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolboxItem {
    /// Select the translate tool.
    Translate,
    /// Select the rotate tool.
    Rotate,
    /// Select the scale tool.
    Scale,
    /// Toggle the gizmo's space between world and local; the label carries
    /// the space it is currently in.
    Space,
    /// Toggle the gizmo's snapping.
    Snap,
    /// Toggle the scene grid under the viewport.
    Grid,
}

/// Every row, in the order the panel stacks them.
pub const TOOLBOX_ITEMS: [ToolboxItem; 6] = [
    ToolboxItem::Translate,
    ToolboxItem::Rotate,
    ToolboxItem::Scale,
    ToolboxItem::Space,
    ToolboxItem::Snap,
    ToolboxItem::Grid,
];

/// The three tools, in the order every surface lists them: the viewport menu
/// and the Preferences row draw these rows, so a new tool is one row here.
pub const TOOL_ROWS: [ToolboxItem; 3] = [
    ToolboxItem::Translate,
    ToolboxItem::Rotate,
    ToolboxItem::Scale,
];

/// One gizmo space as the surfaces that list both spaces draw it: the value,
/// the longer label the viewport menu shows, the short one the Preferences row
/// shows, and the icon both carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpaceRow {
    /// The space this row selects.
    pub space: GizmoSpace,
    /// The label the viewport menu shows.
    pub menu: &'static str,
    /// The label a narrow row shows.
    pub short: &'static str,
    /// The row's icon.
    pub icon: Icon,
}

/// Both spaces, in the order every surface lists them. The toolbox's own space
/// row toggles between the two and takes its label and icon from
/// [`ToolboxItem::label`] and [`ToolboxItem::icon`], which read the same two
/// values.
pub const SPACE_ROWS: [SpaceRow; 2] = [
    SpaceRow {
        space: GizmoSpace::World,
        menu: "World space",
        short: "World",
        icon: Icon::GlobeHemisphereWest,
    },
    SpaceRow {
        space: GizmoSpace::Local,
        menu: "Local space",
        short: "Local",
        icon: Icon::MapPin,
    },
];

impl ToolboxItem {
    /// The row's name in the log and the scripted channel's assertions.
    pub fn name(self) -> &'static str {
        match self {
            ToolboxItem::Translate => "translate",
            ToolboxItem::Rotate => "rotate",
            ToolboxItem::Scale => "scale",
            ToolboxItem::Space => "space",
            ToolboxItem::Snap => "snap",
            ToolboxItem::Grid => "grid",
        }
    }

    /// The row's icon: the tools' own marks, the globe or the map pin for
    /// the space the gizmo is in, and the magnet and grid marks for the
    /// toggles.
    pub fn icon(self, state: &ToolboxState) -> Icon {
        match self {
            ToolboxItem::Translate => Icon::ArrowsOutCardinal,
            ToolboxItem::Rotate => Icon::ArrowsClockwise,
            ToolboxItem::Scale => Icon::ArrowsOut,
            ToolboxItem::Space => match state.gizmo.space {
                GizmoSpace::World => Icon::GlobeHemisphereWest,
                GizmoSpace::Local => Icon::MapPin,
            },
            ToolboxItem::Snap => Icon::Magnet,
            ToolboxItem::Grid => Icon::GridFour,
        }
    }

    /// The row's label for the current state: the tools and the two plain
    /// toggles carry a fixed label, while the space row names the space the
    /// gizmo is in.
    pub fn label(self, state: &ToolboxState) -> &'static str {
        match self {
            ToolboxItem::Translate => "Translate",
            ToolboxItem::Rotate => "Rotate",
            ToolboxItem::Scale => "Scale",
            ToolboxItem::Space => match state.gizmo.space {
                GizmoSpace::World => "World",
                GizmoSpace::Local => "Local",
            },
            ToolboxItem::Snap => "Snap",
            ToolboxItem::Grid => "Grid",
        }
    }

    /// Whether the row reads as active: the selected tool, and each toggle
    /// in its non-default state (local space, snapping on, grid on).
    pub fn active(self, state: &ToolboxState) -> bool {
        match self {
            ToolboxItem::Translate => state.gizmo.mode == GizmoMode::Translate,
            ToolboxItem::Rotate => state.gizmo.mode == GizmoMode::Rotate,
            ToolboxItem::Scale => state.gizmo.mode == GizmoMode::Scale,
            ToolboxItem::Space => state.gizmo.space == GizmoSpace::Local,
            ToolboxItem::Snap => state.gizmo.snap,
            ToolboxItem::Grid => state.grid,
        }
    }

    /// The longer label the viewport menu shows. The tools and the two toggles
    /// read as a menu item; the space row's menu lists the two spaces instead
    /// of this toggle, so it answers the toggle's own name.
    pub fn menu_label(self) -> &'static str {
        match self {
            ToolboxItem::Translate => "Translate",
            ToolboxItem::Rotate => "Rotate",
            ToolboxItem::Scale => "Scale",
            ToolboxItem::Space => "Space",
            ToolboxItem::Snap => "Snapping",
            ToolboxItem::Grid => "Grid",
        }
    }

    /// The command a click queues: the tool rows select, the toggles flip
    /// the state they show.
    pub fn command(self, state: &ToolboxState) -> EditorCommand {
        match self {
            ToolboxItem::Translate => EditorCommand::SetGizmoTool {
                mode: Some(GizmoMode::Translate),
                space: None,
                snap: None,
            },
            ToolboxItem::Rotate => EditorCommand::SetGizmoTool {
                mode: Some(GizmoMode::Rotate),
                space: None,
                snap: None,
            },
            ToolboxItem::Scale => EditorCommand::SetGizmoTool {
                mode: Some(GizmoMode::Scale),
                space: None,
                snap: None,
            },
            ToolboxItem::Space => EditorCommand::SetGizmoTool {
                mode: None,
                space: Some(match state.gizmo.space {
                    GizmoSpace::World => GizmoSpace::Local,
                    GizmoSpace::Local => GizmoSpace::World,
                }),
                snap: None,
            },
            ToolboxItem::Snap => EditorCommand::SetGizmoTool {
                mode: None,
                space: None,
                snap: Some(!state.gizmo.snap),
            },
            ToolboxItem::Grid => EditorCommand::SetGrid {
                enabled: !state.grid,
            },
        }
    }
}

/// The edit that selects one tool: the command the toolbox's row, the viewport
/// menu's row and the Preferences row all queue.
pub fn mode_command(mode: GizmoMode) -> EditorCommand {
    EditorCommand::SetGizmoTool {
        mode: Some(mode),
        space: None,
        snap: None,
    }
}

/// The edit that switches the gizmo to one space.
pub fn space_command(space: GizmoSpace) -> EditorCommand {
    EditorCommand::SetGizmoTool {
        mode: None,
        space: Some(space),
        snap: None,
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use egui::vec2;

    /// A viewport away from the screen's corner, so a clamp bug that falls
    /// back to zero shows.
    fn viewport() -> Rect {
        Rect::from_min_size(Pos2::new(100.0, 50.0), vec2(800.0, 600.0))
    }

    #[test]
    fn the_rows_stack_inside_the_panel() {
        let origin = default_origin(viewport());
        let panel = panel_rect(origin);
        assert!(panel.contains_rect(handle_rect(origin)));
        let mut previous_bottom = handle_rect(origin).bottom();
        for item in TOOLBOX_ITEMS {
            let rect = item_rect(item, origin);
            assert!(panel.contains_rect(rect), "{item:?} outside the panel");
            assert!(
                rect.top() >= previous_bottom,
                "{item:?} overlaps the row above"
            );
            assert_eq!(rect.left(), origin.x + TOOLBOX_PADDING);
            assert_eq!(rect.width(), TOOLBOX_WIDTH - 2.0 * TOOLBOX_PADDING);
            previous_bottom = rect.bottom() + TOOLBOX_ITEM_GAP;
        }
        // The rows end exactly where the panel's bottom padding begins.
        let last = item_rect(ToolboxItem::Grid, origin);
        assert_eq!(panel.bottom(), last.bottom() + TOOLBOX_PADDING);
    }

    #[test]
    fn the_default_origin_sits_at_the_viewports_corner() {
        assert_eq!(default_origin(viewport()), Pos2::new(110.0, 60.0));
    }

    #[test]
    fn a_drag_clamps_inside_the_viewport() {
        let view = viewport();
        let size = panel_size();
        let far = clamped_origin(Pos2::new(5000.0, 5000.0), view);
        assert_eq!(
            far,
            Pos2::new(
                view.right() - size.x - TOOLBOX_MARGIN,
                view.bottom() - size.y - TOOLBOX_MARGIN
            )
        );
        let negative = clamped_origin(Pos2::new(-200.0, -200.0), view);
        assert_eq!(negative, default_origin(view));
        // A point already inside the bounds is left alone.
        let inside = Pos2::new(300.0, 200.0);
        assert_eq!(clamped_origin(inside, view), inside);
    }

    #[test]
    fn a_tiny_viewport_keeps_the_top_left_corner() {
        let tiny = Rect::from_min_size(Pos2::new(100.0, 50.0), vec2(40.0, 40.0));
        let clamped = clamped_origin(Pos2::new(500.0, 500.0), tiny);
        assert_eq!(clamped, default_origin(tiny));
    }

    #[test]
    fn labels_follow_the_settings() {
        let mut state = ToolboxState::default();
        assert_eq!(ToolboxItem::Translate.label(&state), "Translate");
        assert_eq!(ToolboxItem::Space.label(&state), "World");
        state.gizmo.space = GizmoSpace::Local;
        assert_eq!(ToolboxItem::Space.label(&state), "Local");
        assert_eq!(ToolboxItem::Snap.label(&state), "Snap");
        assert_eq!(ToolboxItem::Grid.label(&state), "Grid");
    }

    /// Each row names its own icon; the space row's follows the space the
    /// gizmo is in.
    #[test]
    fn icons_belong_to_their_rows() {
        let mut state = ToolboxState::default();
        assert_eq!(ToolboxItem::Translate.icon(&state), Icon::ArrowsOutCardinal);
        assert_eq!(ToolboxItem::Rotate.icon(&state), Icon::ArrowsClockwise);
        assert_eq!(ToolboxItem::Scale.icon(&state), Icon::ArrowsOut);
        assert_eq!(ToolboxItem::Space.icon(&state), Icon::GlobeHemisphereWest);
        state.gizmo.space = GizmoSpace::Local;
        assert_eq!(ToolboxItem::Space.icon(&state), Icon::MapPin);
        assert_eq!(ToolboxItem::Snap.icon(&state), Icon::Magnet);
        assert_eq!(ToolboxItem::Grid.icon(&state), Icon::GridFour);
    }

    #[test]
    fn active_follows_the_settings() {
        let mut settings = ToolboxState {
            gizmo: GizmoSettings {
                mode: GizmoMode::Scale,
                space: GizmoSpace::Local,
                snap: true,
                ..GizmoSettings::default()
            },
            grid: true,
        };
        assert!(ToolboxItem::Scale.active(&settings));
        assert!(ToolboxItem::Space.active(&settings));
        assert!(ToolboxItem::Snap.active(&settings));
        assert!(ToolboxItem::Grid.active(&settings));
        assert!(!ToolboxItem::Translate.active(&settings));
        settings.grid = false;
        settings.gizmo = GizmoSettings::default();
        assert!(ToolboxItem::Translate.active(&settings));
        assert!(!ToolboxItem::Rotate.active(&settings));
        assert!(!ToolboxItem::Space.active(&settings));
        assert!(!ToolboxItem::Snap.active(&settings));
        assert!(!ToolboxItem::Grid.active(&settings));
    }

    #[test]
    fn a_click_queues_the_edit_that_belongs_to_the_row() {
        let settings = ToolboxState::default();
        assert_eq!(
            ToolboxItem::Rotate.command(&settings),
            EditorCommand::SetGizmoTool {
                mode: Some(GizmoMode::Rotate),
                space: None,
                snap: None,
            }
        );
        assert_eq!(
            ToolboxItem::Space.command(&settings),
            EditorCommand::SetGizmoTool {
                mode: None,
                space: Some(GizmoSpace::Local),
                snap: None,
            }
        );
        assert_eq!(
            ToolboxItem::Snap.command(&settings),
            EditorCommand::SetGizmoTool {
                mode: None,
                space: None,
                snap: Some(true),
            }
        );
        assert_eq!(
            ToolboxItem::Grid.command(&settings),
            EditorCommand::SetGrid { enabled: true }
        );
        // The toggles flip from the state they show.
        let flipped = ToolboxState {
            gizmo: GizmoSettings {
                space: GizmoSpace::Local,
                snap: true,
                ..settings.gizmo
            },
            grid: true,
        };
        assert_eq!(
            ToolboxItem::Space.command(&flipped),
            EditorCommand::SetGizmoTool {
                mode: None,
                space: Some(GizmoSpace::World),
                snap: None,
            }
        );
        assert_eq!(
            ToolboxItem::Snap.command(&flipped),
            EditorCommand::SetGizmoTool {
                mode: None,
                space: None,
                snap: Some(false),
            }
        );
        assert_eq!(
            ToolboxItem::Grid.command(&flipped),
            EditorCommand::SetGrid { enabled: false }
        );
    }

    /// The rows the menu and the Preferences tab draw are the panel's own rows,
    /// in the panel's order, and the spaces they list are the ones the panel's
    /// toggle names: one vocabulary, not three that agree by hand.
    #[test]
    fn the_menu_rows_are_the_panels_rows() {
        assert_eq!(TOOL_ROWS.as_slice(), &TOOLBOX_ITEMS[..3]);
        for item in TOOL_ROWS {
            assert!(TOOLBOX_ITEMS.contains(&item), "{item:?} is not a panel row");
            assert!(!item.menu_label().is_empty(), "{item:?} has no menu label");
        }
        for row in SPACE_ROWS {
            let state = ToolboxState {
                gizmo: GizmoSettings {
                    space: row.space,
                    ..GizmoSettings::default()
                },
                grid: false,
            };
            assert_eq!(ToolboxItem::Space.label(&state), row.short);
            assert_eq!(ToolboxItem::Space.icon(&state), row.icon);
            assert_eq!(
                space_command(row.space),
                EditorCommand::SetGizmoTool {
                    mode: None,
                    space: Some(row.space),
                    snap: None,
                }
            );
        }
        assert_eq!(
            mode_command(GizmoMode::Rotate),
            EditorCommand::SetGizmoTool {
                mode: Some(GizmoMode::Rotate),
                space: None,
                snap: None,
            }
        );
    }
}
