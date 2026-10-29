//! The editor's menu bar: the strip above the dock, its titles, the mark on
//! its left and the entries each menu carries.
//!
//! # Responsibilities
//!
//! - The bar's vocabulary: the menu titles in their order, and the metrics
//!   the shell and the tests share (bar height, the mark's size and inset,
//!   the gap before the first title).
//! - The mark's rectangle inside a bar of a given height - the geometry the
//!   shell positions the texture with.
//! - The menus' contents as plain data: one row per registered command whose
//!   menu path names the menu, and the action a click asks for
//!   ([`MenuAction`]).
//!
//! Nothing here draws: the shell walks [`Menu::ALL`] and egui lays the
//! buttons out, so the bar follows the theme and the font metrics.

// External crates
use egui::{pos2, Rect, Vec2};

// Current crate
use crate::assets;
use crate::commands::CommandView;
use crate::editor_state::{EditorCommand, EditorSelection, EditorSnapshot};
use crate::entities;
use crate::layout::EditorTab;
use crate::systems;
use crate::toolbox::{ToolboxItem, ToolboxState};
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
    /// The scene document's rows, the Preferences tab and quitting the editor.
    File,
    /// History, the clipboard and entity deletion.
    Edit,
    /// A new entity, a new scene and a new asset.
    Create,
    /// The gizmo's tool, space and snapping, the scene grid and the
    /// simulation controls.
    View,
    /// The dock's tabs and one more scene view.
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

/// What the menus are built from: the frame's snapshot, the selection and
/// the registry's command rows.
///
/// The menu model reads only this - never the world - so the drawing and the
/// tests see the same entries from the same values.
pub struct MenuView<'a> {
    /// The engine's captured state: gizmo settings, the grid, asset rows,
    /// the history's depths.
    pub snapshot: &'a EditorSnapshot,
    /// The shared selection the enablement rules read.
    pub selection: &'a EditorSelection,
    /// The registered commands in registration order: a row exists because
    /// one of these names this menu.
    pub commands: &'a [CommandView],
}

/// One row of a menu.
pub enum MenuEntry {
    /// A horizontal rule between groups.
    Separator,
    /// A labeled item.
    Action {
        /// The row's label: the command's title, or the toolbox row's own
        /// label where it carries the current state.
        label: String,
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
    /// Open the same dialog to create a scene, with the scene type chosen.
    CreateScene,
    /// Open the same dialog to write the current world as a scene.
    SaveSceneAs,
    /// Write the selected asset's missing `.meta` sidecar.
    WriteAssetMeta,
    /// Undo the newest recorded step.
    Undo,
    /// Redo the newest undone step.
    Redo,
    /// Bring a dock tab to the front.
    ActivateTab(EditorTab),
    /// Dock one more scene view, with its own camera and toolbox.
    NewSceneView,
    /// Open the About window.
    About,
    /// Quit the editor.
    Quit,
    /// Copy, cut or paste: the shell queues the request the
    /// executable's OS clipboard bridge carries out.
    Clipboard(crate::clipboard::ClipboardAction),
}

/// The entries of `menu`: one row per registered command whose menu path
/// starts with this menu's title, in registration order.
///
/// Nothing here is hard-coded per menu. A command that stops being registered
/// loses its row, a renamed command renames its row, and the rules between
/// groups are the specs marked with
/// [`crate::commands::CommandSpec::separator`].
///
/// A command this model has no action for gets no row: a greyed-out item says
/// "not available right now", which would be a lie about a command nothing
/// implements.
pub fn entries(menu: Menu, view: &MenuView<'_>) -> Vec<MenuEntry> {
    let mut entries = Vec::new();
    for command in view.commands {
        if command.menu_path.first().map(String::as_str) != Some(menu.title()) {
            continue;
        }
        let Some(entry) = row(command, view) else {
            continue;
        };
        if command.separator_before && !entries.is_empty() {
            entries.push(MenuEntry::Separator);
        }
        entries.push(entry);
    }
    entries
}

/// What running one registered command asks the editor to do.
///
/// The palette calls this rather than a menu row's own copy, so a command run
/// from the palette and the same command run from its menu entry reach one
/// action. `None` means this build registers the command but has no action for
/// it: the palette drops the row rather than offering a no-op.
pub fn action_for(
    command: &CommandView,
    snapshot: &EditorSnapshot,
    selection: &EditorSelection,
) -> Option<MenuAction> {
    let view = MenuView {
        snapshot,
        selection,
        commands: std::slice::from_ref(command),
    };
    match row(command, &view)? {
        MenuEntry::Action { action, .. } => Some(action),
        MenuEntry::Separator => None,
    }
}

/// The `.scene` asset the Assets tab has selected, as the path Open Scene
/// loads; `None` when the selection is not a scene file.
fn scene_selection(selection: &EditorSelection) -> Option<String> {
    selection
        .asset
        .as_deref()
        .filter(|path| path.to_ascii_lowercase().ends_with(".scene"))
        .map(str::to_string)
}

/// One row for one registered command: its label, and what a click asks the
/// editor to do.
///
/// The label is the command's title, except where the row carries the current
/// state (the gizmo's space). `None` when the command's id names no action
/// here and no dock tab: that is a gap in this model, not a disabled row, and
/// [`entries`] and [`action_for`] both treat it as such.
fn row(command: &CommandView, view: &MenuView<'_>) -> Option<MenuEntry> {
    let state = ToolboxState {
        gizmo: view.snapshot.gizmo,
        grid: view.snapshot.grid_enabled,
    };
    let mut enabled = true;
    let mut checked = None;
    let mut label = command.title.clone();
    let action = match command.id.as_str() {
        "editor.undo" => {
            enabled = view.snapshot.undo_depth > 0;
            MenuAction::Undo
        }
        "editor.redo" => {
            enabled = view.snapshot.redo_depth > 0;
            MenuAction::Redo
        }
        "editor.entity_delete" => match view.selection.primary() {
            Some(entity) => {
                let (commands, label) =
                    entities::entity_delete(entity, view.selection.asset.clone());
                MenuAction::Queue { label, commands }
            }
            None => {
                enabled = false;
                MenuAction::Inert
            }
        },
        "editor.entity_create" => {
            let (step, label) = entities::entity_create();
            MenuAction::Queue {
                label,
                commands: vec![step],
            }
        }
        // The clipboard rows: Paste is always offered - whether the clipboard
        // holds a payload is the executable's answer, and a refusal there is a
        // toast rather than a greyed row nobody can explain. Copy and Cut read
        // the same predicate the copy itself reads, so a row that is offered
        // is a row with something to carry.
        "editor.copy" | "editor.cut" => {
            let action = if command.id == "editor.copy" {
                crate::clipboard::ClipboardAction::Copy
            } else {
                crate::clipboard::ClipboardAction::Cut
            };
            enabled = crate::clipboard::can_copy(view.selection, view.snapshot);
            MenuAction::Clipboard(action)
        }
        "editor.paste" => MenuAction::Clipboard(crate::clipboard::ClipboardAction::Paste),
        // The scene document's rows. Open acts on the scene
        // selected in the Assets tab, so it greys out until one is; Save needs
        // an open scene file, and Save As asks for a name and a folder in the
        // create dialog, the same window an asset is created in.
        "editor.scene_new" => MenuAction::CreateScene,
        "editor.scene_open" => match scene_selection(view.selection) {
            Some(path) => MenuAction::Queue {
                label: format!("open scene {path}"),
                commands: vec![EditorCommand::SceneOpen { path }],
            },
            None => {
                enabled = false;
                MenuAction::Inert
            }
        },
        "editor.scene_save" => {
            enabled = view.snapshot.scene_open;
            MenuAction::Queue {
                label: "save scene".to_string(),
                commands: vec![EditorCommand::SceneSave],
            }
        }
        "editor.scene_save_as" => MenuAction::SaveSceneAs,
        "editor.asset.create" => MenuAction::CreateAsset,
        "editor.asset_write_meta" => {
            enabled = assets::can_write_meta(view.selection, &view.snapshot.assets);
            MenuAction::WriteAssetMeta
        }
        "editor.preferences_open" => MenuAction::ActivateTab(EditorTab::Preferences),
        "editor.scene_new_view" => MenuAction::NewSceneView,
        "editor.quit" => MenuAction::Quit,
        "editor.about" => MenuAction::About,
        "editor.tool_translate" => {
            let row = toolbox_row(ToolboxItem::Translate, &state);
            label = row.0;
            checked = row.2;
            row.1
        }
        "editor.tool_rotate" => {
            let row = toolbox_row(ToolboxItem::Rotate, &state);
            label = row.0;
            checked = row.2;
            row.1
        }
        "editor.tool_scale" => {
            let row = toolbox_row(ToolboxItem::Scale, &state);
            label = row.0;
            checked = row.2;
            row.1
        }
        "editor.tool_space_toggle" => {
            let row = toolbox_row(ToolboxItem::Space, &state);
            label = row.0;
            checked = row.2;
            row.1
        }
        "editor.tool_snap_toggle" => {
            let row = toolbox_row(ToolboxItem::Snap, &state);
            label = row.0;
            checked = row.2;
            row.1
        }
        "editor.grid_toggle" => {
            let row = toolbox_row(ToolboxItem::Grid, &state);
            label = row.0;
            checked = row.2;
            row.1
        }
        "editor.pause_toggle" => {
            let paused = view.snapshot.paused;
            MenuAction::Queue {
                label: "pause toggle".to_string(),
                commands: vec![systems::pause_toggle_command(paused)],
            }
        }
        "editor.step" => MenuAction::Queue {
            label: "step one frame".to_string(),
            commands: vec![EditorCommand::StepOnce],
        },
        // The Window menu's rows: the tab a `editor.window_<name>` names.
        id => match id
            .strip_prefix("editor.window_")
            .and_then(|name| EditorTab::ALL.into_iter().find(|tab| tab.name() == name))
        {
            Some(tab) => MenuAction::ActivateTab(tab),
            None => return None,
        },
    };
    Some(MenuEntry::Action {
        label,
        action,
        enabled,
        checked,
    })
}

/// A toolbox row as menu data: its state-carrying label, the recorded command
/// a click queues, and whether it reads as active.
fn toolbox_row(item: ToolboxItem, state: &ToolboxState) -> (String, MenuAction, Option<bool>) {
    let label = item.label(state).to_string();
    let action = MenuAction::Queue {
        label: label.clone(),
        commands: vec![item.command(state)],
    };
    (label, action, Some(item.active(state)))
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

    /// The frame's view over one snapshot, selection and command list.
    fn view_over<'a>(
        snapshot: &'a EditorSnapshot,
        selection: &'a EditorSelection,
        commands: &'a [CommandView],
    ) -> MenuView<'a> {
        MenuView {
            snapshot,
            selection,
            commands,
        }
    }

    /// The registered commands, as the frame's snapshot carries them.
    fn commands_fixture() -> Vec<CommandView> {
        crate::commands::CommandRegistry::with_builtins().views()
    }

    fn actions(entries: &[MenuEntry]) -> Vec<(String, bool, Option<bool>)> {
        entries
            .iter()
            .filter_map(|entry| match entry {
                MenuEntry::Separator => None,
                MenuEntry::Action {
                    label,
                    enabled,
                    checked,
                    ..
                } => Some((label.clone(), *enabled, *checked)),
            })
            .collect()
    }

    /// The labels only, for the menus whose enablement another test covers.
    fn labels(entries: &[MenuEntry]) -> Vec<String> {
        actions(entries).into_iter().map(|row| row.0).collect()
    }

    fn separators(entries: &[MenuEntry]) -> usize {
        entries
            .iter()
            .filter(|entry| matches!(entry, MenuEntry::Separator))
            .count()
    }

    #[test]
    fn the_file_menu_offers_the_scene_rows_the_preferences_tab_and_quitting() {
        let commands = commands_fixture();
        let snapshot = snapshot_fixture();
        let selection = EditorSelection::default();
        let entries = entries(Menu::File, &view_over(&snapshot, &selection, &commands));
        // Three rules split the rows: Open/Save, then Save As with the sidecar
        // write, then the Preferences tab, then Quit.
        assert_eq!(
            labels(&entries),
            vec![
                "Open Scene",
                "Save Scene",
                "Save Scene As",
                "Write Asset Meta",
                "Editor preferences",
                "Quit Editor",
            ]
        );
        // The empty selection is what greys the sidecar write out.
        assert!(!actions(&entries)[3].1);
        assert!(actions(&entries)[4].1, "the preferences tab always opens");
        assert_eq!(separators(&entries), 3);
        assert!(matches!(entries[2], MenuEntry::Separator));
        assert!(matches!(entries[5], MenuEntry::Separator));
        assert!(matches!(entries[7], MenuEntry::Separator));
    }

    #[test]
    fn the_edit_menu_follows_the_history_depths_and_the_selection() {
        let commands = commands_fixture();
        let mut snapshot = snapshot_fixture();
        let mut selection = EditorSelection::default();
        let disabled = entries(Menu::Edit, &view_over(&snapshot, &selection, &commands));
        assert_eq!(
            labels(&disabled),
            vec!["Undo", "Redo", "Copy", "Cut", "Paste", "Delete Entity"]
        );
        let rows = actions(&disabled);
        assert!(!rows[0].1, "no undo history yet");
        assert!(!rows[1].1, "no redo history yet");
        assert!(!rows[5].1, "nothing selected to delete");
        // Copy and cut need something to carry; paste is always offered,
        // because whether the clipboard holds a payload is the executable's
        // answer.
        assert_eq!((rows[2].1, rows[3].1), (false, false));
        assert!(rows[4].1, "paste is offered with nothing selected");

        snapshot.undo_depth = 2;
        snapshot.redo_depth = 1;
        let mut engine = pill_engine::Engine::new();
        let entity = engine
            .world_mut()
            .create_entity()
            .build()
            .expect("entity builds");
        selection.select_only(entity);
        let enabled = entries(Menu::Edit, &view_over(&snapshot, &selection, &commands));
        let rows = actions(&enabled);
        assert!(rows[0].1);
        assert!(rows[1].1);
        assert!(rows[5].1);
        // An entity selected on a snapshot that has not captured it yet has
        // nothing to carry, so the clipboard rows stay grey: they used to be
        // enabled here and the copy then carried the asset the selection
        // replaced - or nothing at all.
        assert_eq!((rows[2].1, rows[3].1), (false, false));
        snapshot.detail = Some(crate::editor_state::EntityDetail {
            entity,
            components: Vec::new(),
        });
        let captured = entries(Menu::Edit, &view_over(&snapshot, &selection, &commands));
        let rows = actions(&captured);
        // A captured detail is something to copy and cut.
        assert_eq!((rows[2].1, rows[3].1), (true, true));
        match &enabled[6] {
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
        let commands = commands_fixture();
        let snapshot = snapshot_fixture();
        let selection = EditorSelection::default();
        let menu = entries(Menu::Create, &view_over(&snapshot, &selection, &commands));
        // The entity create, then the two asset rows the person asked to move
        // here: a scene and a standalone asset.
        assert_eq!(
            labels(&menu),
            vec!["New Entity", "Create Scene", "Create Asset..."]
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
        let commands = commands_fixture();
        let snapshot = snapshot_fixture();
        let selection = EditorSelection::default();
        let menu = entries(Menu::View, &view_over(&snapshot, &selection, &commands));
        // The toolbox's six rows in the panel's order, then the simulation
        // controls behind the last rule.
        assert_eq!(
            labels(&menu),
            vec![
                "Translate",
                "Rotate",
                "Scale",
                "World",
                "Snap",
                "Grid",
                "Pause / Resume Simulation",
                "Step One Frame",
            ]
        );
        let rows = actions(&menu);
        // The default gizmo mode is translate, in world space, unsnapped.
        assert_eq!(rows[0].2, Some(true));
        assert_eq!(rows[1].2, Some(false));
        assert_eq!(rows[3].2, Some(false));
        // The grid row follows the renderer's switch, off on a fresh engine.
        assert_eq!(rows[5].2, Some(snapshot.grid_enabled));
        // Three rules: before the space row, before the grid row, and before
        // the simulation controls.
        assert_eq!(separators(&menu), 3);
        // The rows that toggle carry a check state; the two simulation rows
        // are plain actions.
        assert!(rows[..6].iter().all(|row| row.2.is_some()));
        assert!(rows[6..].iter().all(|row| row.2.is_none()));
    }

    #[test]
    fn the_window_menu_lists_every_tab_in_strip_order() {
        let commands = commands_fixture();
        let snapshot = snapshot_fixture();
        let selection = EditorSelection::default();
        let menu = entries(Menu::Window, &view_over(&snapshot, &selection, &commands));
        // The new-view row comes first, then one row per
        // tab in strip order.
        let mut expected: Vec<String> = vec!["New Scene View".to_string()];
        expected.extend(EditorTab::ALL.iter().map(|tab| tab.title()));
        assert_eq!(labels(&menu), expected);
        assert!(matches!(
            menu[0],
            MenuEntry::Action {
                action: MenuAction::NewSceneView,
                ..
            }
        ));
        for (entry, tab) in menu.iter().skip(1).zip(EditorTab::ALL.iter()) {
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
        let commands = commands_fixture();
        let snapshot = snapshot_fixture();
        let selection = EditorSelection::default();
        let menu = entries(Menu::Help, &view_over(&snapshot, &selection, &commands));
        assert_eq!(labels(&menu), vec!["About Pill Editor"]);
        assert!(matches!(
            menu[0],
            MenuEntry::Action {
                action: MenuAction::About,
                ..
            }
        ));
    }

    #[test]
    fn every_command_a_drawn_menu_lists_has_an_action() {
        let commands = commands_fixture();
        let snapshot = snapshot_fixture();
        let selection = EditorSelection::default();
        let titles: Vec<&str> = Menu::ALL.iter().map(|menu| menu.title()).collect();
        for command in &commands {
            let Some(top) = command.menu_path.first() else {
                continue;
            };
            if !titles.contains(&top.as_str()) {
                continue;
            }
            assert!(
                action_for(command, &snapshot, &selection).is_some(),
                "{} is registered under the {} menu, which the bar draws, but \
                 no action answers it",
                command.id,
                top
            );
        }
    }

    #[test]
    fn no_registered_menu_path_names_a_menu_the_bar_cannot_draw() {
        let titles: Vec<&str> = Menu::ALL.iter().map(|menu| menu.title()).collect();
        for command in commands_fixture() {
            if let Some(top) = command.menu_path.first() {
                assert!(
                    titles.contains(&top.as_str()),
                    "{} names the {} menu, which Menu::ALL does not carry",
                    command.id,
                    top
                );
            }
        }
    }

    #[test]
    fn a_command_without_an_action_draws_no_row_and_runs_nothing() {
        let mut commands = commands_fixture();
        commands.push(CommandView {
            id: "editor.unimplemented".to_string(),
            title: "Unimplemented".to_string(),
            menu_path: vec!["Edit".to_string(), "Unimplemented".to_string()],
            separator_before: false,
        });
        let snapshot = snapshot_fixture();
        let selection = EditorSelection::default();
        let menu = entries(Menu::Edit, &view_over(&snapshot, &selection, &commands));
        assert!(
            !labels(&menu).iter().any(|label| label == "Unimplemented"),
            "a command with no action must not draw as a row that does nothing"
        );
        let unknown = commands
            .iter()
            .find(|command| command.id == "editor.unimplemented")
            .expect("the fixture pushed it");
        assert_eq!(action_for(unknown, &snapshot, &selection), None);
    }

    #[test]
    fn dropping_a_command_from_the_registry_drops_its_row() {
        let kept: Vec<CommandView> = commands_fixture()
            .into_iter()
            .filter(|command| command.id != "editor.undo")
            .collect();
        let snapshot = snapshot_fixture();
        let selection = EditorSelection::default();
        let menu = entries(Menu::Edit, &view_over(&snapshot, &selection, &kept));
        assert!(
            !labels(&menu).iter().any(|label| label == "Undo"),
            "the row must follow the registry, not a table in the shell"
        );
    }

    #[test]
    fn a_renamed_command_renames_its_row() {
        let mut commands = commands_fixture();
        let undo = commands
            .iter_mut()
            .find(|command| command.id == "editor.undo")
            .expect("the built-ins register undo");
        undo.title = "Step Back".to_string();
        let snapshot = snapshot_fixture();
        let selection = EditorSelection::default();
        let menu = entries(Menu::Edit, &view_over(&snapshot, &selection, &commands));
        assert_eq!(labels(&menu)[0], "Step Back");
    }

    /// A snapshot over a fresh engine, which the entry tests take views of.
    fn snapshot_fixture() -> EditorSnapshot {
        let engine = pill_engine::Engine::new();
        EditorSnapshot::capture_list(&engine, 0, &[], Vec::new())
    }
}
