//! The editor's menu bar: the strip above the dock, its six titles and the
//! mark on its left.
//!
//! # Responsibilities
//!
//! - The bar's vocabulary: the menu titles in their order, and the metrics
//!   the shell and the tests share (bar height, the mark's size and inset,
//!   title padding).
//! - The mark's rectangle inside a bar of a given height - the geometry the
//!   shell positions the texture with.
//! - The menu model itself (what each menu contains and which editor action
//!   an item runs) is [`crate::menubar`]'s job; this module only says what
//!   the bar looks like.
//!
//! Nothing here draws: the shell walks [`Menu::ALL`] and egui lays the
//! buttons out, so the bar follows the theme and the font metrics.

// External crates
use pill_ui_egui::egui::{pos2, Rect, Vec2};

// Current crate
use crate::assets;
use crate::editor_state::{EditorCommand, EditorSelection, EditorSnapshot};
use crate::entities;
use crate::layout::EditorTab;
use crate::toolbox::{self, ToolboxState};
/// The bar's height in points: one row of menu buttons with the mark inside.
pub const MENU_BAR_HEIGHT: f32 = 30.0;

/// The mark's edge, in points.
pub const MENU_MARK_SIDE: f32 = 18.0;

/// The gap between the bar's left edge and the mark.
pub const MENU_MARK_LEFT: f32 = 10.0;

/// The gap between the mark and the first title.
pub const MENU_MARK_GAP: f32 = 8.0;

/// How many titles the bar carries, for the logged-geometry arrays.
pub const MENU_TITLE_COUNT: usize = Menu::ALL.len();

/// One menu title: the strip's entry, in the order it is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Menu {
    /// Project-level entries: assets and quitting the editor.
    File,
    /// History and selection edits.
    Edit,
    /// New entities and assets.
    Create,
    /// The gizmo, the space, snapping and the scene grid.
    View,
    /// The dock's tabs.
    Window,
    /// The About window.
    Help,
}

impl Menu {
    /// Every title, in the order they follow the mark.
    pub const ALL: [Menu; 6] = [
        Menu::File,
        Menu::Edit,
        Menu::Create,
        Menu::View,
        Menu::Window,
        Menu::Help,
    ];

    /// The title's label, as the bar draws it.
    pub fn title(self) -> &'static str {
        match self {
            Menu::File => "File",
            Menu::Edit => "Edit",
            Menu::Create => "Create",
            Menu::View => "View",
            Menu::Window => "Window",
            Menu::Help => "Help",
        }
    }

    /// The wire name the log and the scripted channel use.
    pub fn name(self) -> &'static str {
        match self {
            Menu::File => "file",
            Menu::Edit => "edit",
            Menu::Create => "create",
            Menu::View => "view",
            Menu::Window => "window",
            Menu::Help => "help",
        }
    }
}

/// The mark's rectangle in a bar of `height` points: square, inset by
/// [`MENU_MARK_LEFT`] on the left and centred vertically.
pub fn mark_rect(height: f32) -> Rect {
    let top = ((height - MENU_MARK_SIDE) * 0.5).max(0.0);
    Rect::from_min_size(pos2(MENU_MARK_LEFT, top), Vec2::splat(MENU_MARK_SIDE))
}

/// What the menus are built from: the frame's snapshot and selection.
///
/// The menu model reads only this - never the world - so the drawing and the
/// tests see the same entries from the same values.
pub struct MenuView<'a> {
    /// The engine's captured state: gizmo settings, the grid, asset rows,
    /// the history's depths.
    pub snapshot: &'a EditorSnapshot,
    /// The shared selection the enablement rules read.
    pub selection: &'a EditorSelection,
}

/// One row of a menu.
pub enum MenuEntry {
    /// A horizontal rule between groups.
    Separator,
    /// A labeled item.
    Action {
        /// The row's label.
        label: &'static str,
        /// What a click asks the editor to do.
        action: MenuAction,
        /// Whether the item can act; a disabled row draws greyed out.
        enabled: bool,
        /// `Some(on)` for a toggle or radio row (the theme highlights it),
        /// `None` for a plain item.
        checked: Option<bool>,
    },
}

/// What a menu item asks the editor to do, as plain data.
///
/// The shell turns an action into the same calls the tabs, the toolbox and
/// the scripted channel make: nothing here touches the world.
#[derive(Clone, Debug, PartialEq)]
pub enum MenuAction {
    /// Nothing: the row's action does not apply (a disabled row).
    Inert,
    /// Queue a recorded step: the label it records under and its commands.
    Queue {
        /// The history label the step records under.
        label: String,
        /// The commands the step carries.
        commands: Vec<EditorCommand>,
    },
    /// Open the create-asset dialog.
    CreateAsset,
    /// Write the selected asset's missing `.meta` sidecar.
    WriteAssetMeta,
    /// Undo the newest recorded step.
    Undo,
    /// Redo the newest undone step.
    Redo,
    /// Bring a dock tab to the front.
    ActivateTab(EditorTab),
    /// Open the About window.
    About,
    /// Quit the editor.
    Quit,
}

/// The entries of `menu`, in order, for the frame's view.
pub fn entries(menu: Menu, view: &MenuView<'_>) -> Vec<MenuEntry> {
    match menu {
        Menu::File => file_entries(view),
        Menu::Edit => edit_entries(view),
        Menu::Create => create_entries(),
        Menu::View => view_entries(view),
        Menu::Window => window_entries(),
        Menu::Help => help_entries(),
    }
}

/// `File`: the asset file actions and quitting.
fn file_entries(view: &MenuView<'_>) -> Vec<MenuEntry> {
    let selected_row = assets::selected_row(view.selection, &view.snapshot.assets);
    // The toolbar's rule: only a sourced file without a sidecar can take one.
    let needs_meta =
        selected_row.is_some_and(|row| row.kind != "folder" && !row.standalone && !row.has_meta);
    vec![
        MenuEntry::Action {
            label: "Create Asset...",
            action: MenuAction::CreateAsset,
            enabled: true,
            checked: None,
        },
        MenuEntry::Action {
            label: "Write Asset Meta",
            action: MenuAction::WriteAssetMeta,
            enabled: needs_meta,
            checked: None,
        },
        MenuEntry::Separator,
        MenuEntry::Action {
            label: "Quit Editor",
            action: MenuAction::Quit,
            enabled: true,
            checked: None,
        },
    ]
}

/// `Edit`: history, the selected entity's delete, and the preferences tab.
fn edit_entries(view: &MenuView<'_>) -> Vec<MenuEntry> {
    let delete_action = match view.selection.entity {
        Some(entity) => {
            let (commands, label) = entities::entity_delete(entity, view.selection.asset.clone());
            MenuAction::Queue { label, commands }
        }
        None => MenuAction::Inert,
    };
    vec![
        MenuEntry::Action {
            label: "Undo",
            action: MenuAction::Undo,
            enabled: view.snapshot.undo_depth > 0,
            checked: None,
        },
        MenuEntry::Action {
            label: "Redo",
            action: MenuAction::Redo,
            enabled: view.snapshot.redo_depth > 0,
            checked: None,
        },
        MenuEntry::Separator,
        MenuEntry::Action {
            label: "Delete Entity",
            action: delete_action,
            enabled: view.selection.entity.is_some(),
            checked: None,
        },
        MenuEntry::Separator,
        MenuEntry::Action {
            label: "Preferences...",
            action: MenuAction::ActivateTab(EditorTab::Preferences),
            enabled: true,
            checked: None,
        },
    ]
}

/// `Create`: a new entity and the asset dialog.
fn create_entries() -> Vec<MenuEntry> {
    let (command, label) = entities::entity_create();
    vec![
        MenuEntry::Action {
            label: "New Entity",
            action: MenuAction::Queue {
                label,
                commands: vec![command],
            },
            enabled: true,
            checked: None,
        },
        MenuEntry::Action {
            label: "Create Asset...",
            action: MenuAction::CreateAsset,
            enabled: true,
            checked: None,
        },
    ]
}

/// `View`: the toolbox's own rows - the gizmo tools, the space, snapping and
/// the scene grid - so the bar and the floating panel cannot disagree.
fn view_entries(view: &MenuView<'_>) -> Vec<MenuEntry> {
    let state = ToolboxState {
        gizmo: view.snapshot.gizmo,
        grid: view.snapshot.grid_enabled,
    };
    let mut entries = Vec::new();
    for (index, item) in toolbox::TOOLBOX_ITEMS.iter().enumerate() {
        if index == 3 || index == 5 {
            entries.push(MenuEntry::Separator);
        }
        entries.push(MenuEntry::Action {
            label: item.label(&state),
            action: MenuAction::Queue {
                label: item.label(&state).to_string(),
                commands: vec![item.command(&state)],
            },
            enabled: true,
            checked: Some(item.active(&state)),
        });
    }
    entries
}

/// `Window`: one row per dock tab, in strip order.
fn window_entries() -> Vec<MenuEntry> {
    EditorTab::ALL
        .iter()
        .map(|tab| MenuEntry::Action {
            label: tab.title(),
            action: MenuAction::ActivateTab(*tab),
            enabled: true,
            checked: None,
        })
        .collect()
}

/// `Help`: the About window.
fn help_entries() -> Vec<MenuEntry> {
    vec![MenuEntry::Action {
        label: "About Pill Editor",
        action: MenuAction::About,
        enabled: true,
        checked: None,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_titles_keep_unity_order_after_the_mark() {
        let titles: Vec<&str> = Menu::ALL.iter().map(|menu| menu.title()).collect();
        assert_eq!(
            titles,
            vec!["File", "Edit", "Create", "View", "Window", "Help"]
        );
    }

    #[test]
    fn titles_and_wire_names_are_distinct() {
        for menu in Menu::ALL {
            assert!(!menu.title().is_empty());
            assert_eq!(menu.name(), menu.title().to_lowercase());
        }
    }

    #[test]
    fn the_mark_sits_left_and_centres_in_the_bar() {
        let rect = mark_rect(MENU_BAR_HEIGHT);
        assert_eq!(rect.width(), MENU_MARK_SIDE);
        assert_eq!(rect.height(), MENU_MARK_SIDE);
        assert_eq!(rect.min.x, MENU_MARK_LEFT);
        let bar_center = MENU_BAR_HEIGHT * 0.5;
        assert!((rect.center().y - bar_center).abs() < 0.01);
    }

    #[test]
    fn a_bar_shorter_than_the_mark_keeps_the_mark_inside() {
        let rect = mark_rect(10.0);
        assert_eq!(rect.min.y, 0.0);
    }

    /// The frame's view over a fresh engine, with the history depths the
    /// menu's enablement reads.
    fn view_over<'a>(
        snapshot: &'a mut EditorSnapshot,
        selection: &'a EditorSelection,
    ) -> MenuView<'a> {
        MenuView {
            snapshot,
            selection,
        }
    }

    fn actions(entries: &[MenuEntry]) -> Vec<(&'static str, bool, Option<bool>)> {
        entries
            .iter()
            .filter_map(|entry| match entry {
                MenuEntry::Separator => None,
                MenuEntry::Action {
                    label,
                    enabled,
                    checked,
                    ..
                } => Some((*label, *enabled, *checked)),
            })
            .collect()
    }

    #[test]
    fn the_file_menu_offers_the_asset_actions_and_quitting() {
        let engine = pill_engine::Engine::new();
        let mut snapshot = EditorSnapshot::capture_list(&engine, 0, &[], Vec::new());
        let selection = EditorSelection::default();
        let entries = entries(Menu::File, &view_over(&mut snapshot, &selection));
        assert_eq!(
            actions(&entries),
            vec![
                ("Create Asset...", true, None),
                ("Write Asset Meta", false, None),
                ("Quit Editor", true, None),
            ]
        );
        assert!(matches!(entries[2], MenuEntry::Separator));
    }

    #[test]
    fn the_edit_menu_follows_the_history_depths_and_the_selection() {
        let engine = pill_engine::Engine::new();
        let mut snapshot = EditorSnapshot::capture_list(&engine, 0, &[], Vec::new());
        let mut selection = EditorSelection::default();
        let disabled = entries(Menu::Edit, &view_over(&mut snapshot, &selection));
        assert_eq!(
            actions(&disabled),
            vec![
                ("Undo", false, None),
                ("Redo", false, None),
                ("Delete Entity", false, None),
                ("Preferences...", true, None),
            ]
        );

        snapshot.undo_depth = 2;
        snapshot.redo_depth = 1;
        let mut engine = pill_engine::Engine::new();
        let entity = engine
            .world_mut()
            .create_entity()
            .build()
            .expect("entity builds");
        selection.entity = Some(entity);
        let enabled = entries(Menu::Edit, &view_over(&mut snapshot, &selection));
        assert_eq!(
            actions(&enabled),
            vec![
                ("Undo", true, None),
                ("Redo", true, None),
                ("Delete Entity", true, None),
                ("Preferences...", true, None),
            ]
        );
        match &enabled[3] {
            MenuEntry::Action {
                action: MenuAction::Queue { label, commands },
                ..
            } => {
                assert_eq!(label, "delete entity");
                assert_eq!(commands.len(), 2);
            }
            _ => panic!("the delete row must queue its recorded step"),
        }
    }

    #[test]
    fn the_create_menu_builds_one_empty_entity() {
        let menu = entries(
            Menu::Create,
            &MenuView {
                snapshot: &snapshot_fixture(),
                selection: &EditorSelection::default(),
            },
        );
        assert_eq!(
            actions(&menu),
            vec![("New Entity", true, None), ("Create Asset...", true, None)]
        );
        match &menu[0] {
            MenuEntry::Action {
                action: MenuAction::Queue { label, commands },
                ..
            } => {
                assert_eq!(label, "new entity");
                assert_eq!(
                    commands,
                    &vec![EditorCommand::CreateEntity {
                        components: Vec::new()
                    }]
                );
            }
            _ => panic!("the create row must queue the recorded step"),
        }
    }

    #[test]
    fn the_view_menu_mirrors_the_toolbox_rows_and_checks_the_active_ones() {
        let snapshot = snapshot_fixture();
        let selection = EditorSelection::default();
        let menu = entries(
            Menu::View,
            &view_over(&mut { snapshot_fixture() }, &selection),
        );
        let rows = actions(&menu);
        let outline: Vec<&str> = toolbox::TOOLBOX_ITEMS
            .iter()
            .map(|item| {
                item.label(&ToolboxState {
                    gizmo: snapshot.gizmo,
                    grid: snapshot.grid_enabled,
                })
            })
            .collect();
        assert_eq!(rows.len(), outline.len());
        for (row, expected) in rows.iter().zip(outline.iter()) {
            assert_eq!(row.0, *expected);
            assert!(row.1);
            assert!(row.2.is_some(), "every view row is a toggle");
        }
        // The default gizmo mode is translate; the grid row follows the
        // renderer's switch, which a fresh engine leaves off.
        let checked: Vec<&str> = rows
            .iter()
            .filter(|row| row.2 == Some(true))
            .map(|row| row.0)
            .collect();
        assert!(checked.contains(&"Translate"));
        let grid_row = rows.iter().find(|row| row.0 == "Grid").expect("grid row");
        assert_eq!(grid_row.2, Some(snapshot.grid_enabled));
        // Two separators group tools, space/snap and the grid.
        let separators = menu
            .iter()
            .filter(|entry| matches!(entry, MenuEntry::Separator))
            .count();
        assert_eq!(separators, 2);
    }

    #[test]
    fn the_window_menu_lists_every_tab_in_strip_order() {
        let menu = entries(
            Menu::Window,
            &MenuView {
                snapshot: &snapshot_fixture(),
                selection: &EditorSelection::default(),
            },
        );
        let labels: Vec<&str> = actions(&menu).iter().map(|row| row.0).collect();
        let expected: Vec<&str> = EditorTab::ALL.iter().map(|tab| tab.title()).collect();
        assert_eq!(labels, expected);
        for (entry, tab) in menu.iter().zip(EditorTab::ALL.iter()) {
            match entry {
                MenuEntry::Action {
                    action: MenuAction::ActivateTab(activated),
                    ..
                } => assert_eq!(activated, tab),
                _ => panic!("a window row must activate its tab"),
            }
        }
    }

    #[test]
    fn the_help_menu_opens_about() {
        let menu = entries(
            Menu::Help,
            &MenuView {
                snapshot: &snapshot_fixture(),
                selection: &EditorSelection::default(),
            },
        );
        assert_eq!(actions(&menu), vec![("About Pill Editor", true, None)]);
        assert!(matches!(
            menu[0],
            MenuEntry::Action {
                action: MenuAction::About,
                ..
            }
        ));
    }

    /// A snapshot over a fresh engine, cloned for the entries that take a
    /// view of it.
    fn snapshot_fixture() -> EditorSnapshot {
        let engine = pill_engine::Engine::new();
        EditorSnapshot::capture_list(&engine, 0, &[], Vec::new())
    }
}
