//! The menu bar: the menus built from the command rows, and the actions they run.
//!
//! # Responsibilities
//!
//! - The bar's menus and their items, drawn from the shell's mark and the frame's menu model.
//! - The actions a menu item runs (`File > Quit Editor`, the Window tab rows,
//!   the gizmo rows), all through [`run_menu_action`], which the command
//!   palette also calls.
//! - The About window the Help menu opens.
//!
//! The globs below are absolute paths, so a patch crate resolves them too.

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;
use pill_editor_shell_egui_editor::shell::*;

// =============================================================================
// Constants
// =============================================================================

/// The gap between two menu titles in the bar, in points.
pub const MENU_TITLE_GAP: f32 = 2.0;

// =============================================================================
// Free Functions
// =============================================================================

/// Draws the menu bar above the dock: the pill mark, then Unity's six titles.
///
/// The bar is a top panel drawn before the dock, so the dock and every viewport
/// rectangle the frame reports start below it. Each title's rectangle is
/// logged once per placement, and an open menu's entries log theirs the same
/// way, so a suite can click a menu row without knowing the font or the theme
/// metrics.
///
/// Public because a hot patch of [`run_frame`] calls it through the package
/// path, like every other function a patch can reach.
pub fn menu_bar(
    ui: &mut egui::Ui,
    session: &mut MenubarSession,
    mark: &egui::TextureHandle,
    viewer: &mut EditorTabViewer<'_>,
) {
    let style = ui.ctx().global_style();
    let mark_tint = style.visuals.text_color();
    egui::Panel::top("pill_editor_shell_egui.menu_bar")
        .exact_size(menubar::MENU_BAR_HEIGHT)
        .frame(
            egui::Frame::side_top_panel(style.as_ref()).inner_margin(egui::Margin::symmetric(0, 4)),
        )
        .show(ui, |ui| {
            // Step 1: Log the strip's rectangle when it moved.
            let bar_rect = ui.max_rect();
            let mut open_menu: Option<menubar::Menu> = None;
            if session.geometry.remember_exact(MenubarSlot::Bar, bar_rect) {
                pill_core::debug!(
                    target: telemetry_target::EDITOR,
                    x = bar_rect.min.x,
                    y = bar_rect.min.y,
                    width = bar_rect.width(),
                    height = bar_rect.height(),
                    "[editor] menu bar rect"
                );
            }
            // Step 2: Draw the mark and the titles, logging each title's
            // rectangle when it moved.
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                // Unity's strip: plain labels at rest, a wash under the
                // pointer or the open menu - not resting buttons.
                ui.visuals_mut().widgets.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
                ui.add_space(menubar::MENU_MARK_LEFT);
                let mark_size = egui::vec2(menubar::MENU_MARK_SIDE, menubar::MENU_MARK_SIDE);
                ui.add(egui::Image::new((mark.id(), mark_size)).tint(mark_tint));
                ui.add_space(menubar::MENU_MARK_GAP);
                for (index, menu) in menubar::Menu::ALL.iter().enumerate() {
                    if index > 0 {
                        ui.add_space(MENU_TITLE_GAP);
                    }
                    let title_button = ui.menu_button(menu.title(), |ui| {
                        menu_body(ui, *menu, session, viewer);
                    });
                    if title_button.inner.is_some() {
                        open_menu = Some(*menu);
                    }
                    let title_response = title_button.response;
                    if session
                        .geometry
                        .remember_exact(MenubarSlot::Title(index), title_response.rect)
                    {
                        pill_core::debug!(
                            target: telemetry_target::EDITOR,
                            name = menu.name(),
                            x = title_response.rect.min.x,
                            y = title_response.rect.min.y,
                            width = title_response.rect.width(),
                            height = title_response.rect.height(),
                            "[editor] menu item rect"
                        );
                    }
                }
            });
            // Step 3: Remember which menu is open, and log a change once.
            if let Some(menu) = open_menu {
                if session.open != Some(menu) {
                    session.open = Some(menu);
                    pill_core::info!(
                        target: telemetry_target::EDITOR,
                        menu = menu.name(),
                        "[editor] menu opened"
                    );
                }
            } else {
                session.open = None;
            }
        });
}

/// Draws one open menu: the rows its model returns, each row's rectangle in
/// the placement log, and the action a click runs.
pub fn menu_body(
    ui: &mut egui::Ui,
    menu: menubar::Menu,
    session: &mut MenubarSession,
    viewer: &mut EditorTabViewer<'_>,
) {
    // Step 1: Build the view the model reads and draw the rows it returns.
    let view = menubar::MenuView {
        snapshot: viewer.snapshot,
        selection: viewer.selection,
        commands: &viewer.snapshot.commands,
    };
    for entry in menubar::entries(menu, &view) {
        match entry {
            menubar::MenuEntry::Separator => {
                ui.separator();
            }
            menubar::MenuEntry::Action {
                label,
                action,
                enabled,
                checked,
            } => {
                let row_response = ui.add_enabled(
                    enabled,
                    egui::Button::selectable(checked.unwrap_or(false), label.as_str()),
                );
                let entry_key = format!("{}/{}", menu.name(), label);
                if session.entries.remember_exact(entry_key, row_response.rect) {
                    pill_core::debug!(
                        target: telemetry_target::EDITOR,
                        menu = menu.name(),
                        label,
                        x = row_response.rect.min.x,
                        y = row_response.rect.min.y,
                        width = row_response.rect.width(),
                        height = row_response.rect.height(),
                        "[editor] menu entry rect"
                    );
                }
                if row_response.clicked() {
                    run_menu_action(menu.name(), label.as_str(), action, session, viewer);
                    ui.close();
                }
            }
        }
    }
    // Step 2: Record the popup's rectangle as an overlay region, so a click on
    // it is the UI's and never a scene pick.
    let popup_rect = ui.min_rect();
    viewer.overlay_regions.push(ShellRect {
        x: popup_rect.min.x,
        y: popup_rect.min.y,
        width: popup_rect.width(),
        height: popup_rect.height(),
    });
}

/// Runs one menu item through the same calls the tabs, the toolbox and the
/// scripted channel make.
///
/// `menu_name` is the name the menu logs itself under (`file`, `window`, ...,
/// or `palette`), so every surface that runs a command logs the same field.
pub fn run_menu_action(
    menu_name: &str,
    label: &str,
    action: menubar::MenuAction,
    session: &mut MenubarSession,
    viewer: &mut EditorTabViewer<'_>,
) {
    // Step 1: Run the action the row carries.
    match action {
        menubar::MenuAction::Inert => {}
        menubar::MenuAction::Queue {
            label: history_label,
            commands,
        } => {
            viewer.pending.push_recorded(commands, history_label, None);
        }
        menubar::MenuAction::CreateAsset => {
            assets::open_create(
                viewer.asset_dialog,
                viewer.selection,
                &viewer.snapshot.assets,
                &viewer.snapshot.standalone_types,
            );
        }
        menubar::MenuAction::CreateScene => {
            assets::open_create_scene(
                viewer.asset_dialog,
                viewer.selection,
                &viewer.snapshot.assets,
                &viewer.snapshot.standalone_types,
                assets::CreateKind::EmptyScene,
            );
        }
        menubar::MenuAction::SaveSceneAs => {
            assets::open_create_scene(
                viewer.asset_dialog,
                viewer.selection,
                &viewer.snapshot.assets,
                &viewer.snapshot.standalone_types,
                assets::CreateKind::SaveWorld,
            );
        }
        menubar::MenuAction::WriteAssetMeta => {
            assets::write_meta_selection(viewer.pending, viewer.selection, &viewer.snapshot.assets);
        }
        menubar::MenuAction::Undo => viewer
            .requests
            .history
            .push(pill_editor_core::requests::HistoryRequest::Undo),
        menubar::MenuAction::Redo => viewer
            .requests
            .history
            .push(pill_editor_core::requests::HistoryRequest::Redo),
        menubar::MenuAction::ActivateTab(tab) => viewer.requests.pending_tab = Some(tab),
        // The dock is borrowed by the `DockArea` while this menu draws, so the
        // new view is docked after the pass.
        menubar::MenuAction::NewSceneView => viewer.deferred.new_scene_view = true,
        menubar::MenuAction::About => session.about_open = true,
        menubar::MenuAction::Quit => viewer.deferred.quit_requested = true,
        // The shell cannot reach the OS clipboard. This call builds the
        // payload and queues a request; the executable carries it out at the
        // frame boundary.
        menubar::MenuAction::Clipboard(clipboard_action) => {
            let payload_summary = pill_editor_core::clipboard::queue_action(
                viewer.clipboard,
                clipboard_action,
                viewer.selection,
                viewer.snapshot,
                viewer.pending,
            );
            pill_core::info!(
                target: telemetry_target::EDITOR,
                action = match clipboard_action {
                    pill_editor_core::clipboard::ClipboardAction::Copy => "copy",
                    pill_editor_core::clipboard::ClipboardAction::Cut => "cut",
                    pill_editor_core::clipboard::ClipboardAction::Paste => "paste",
                },
                payload = payload_summary.as_deref().unwrap_or("nothing"),
                "[editor] clipboard queued"
            );
        }
    }
    // Step 2: Log which menu ran which row, so a suite can follow the surface.
    pill_core::info!(
        target: telemetry_target::EDITOR,
        menu = menu_name,
        label,
        "[editor] menu action"
    );
}

/// Draws the About window: the mark, the editor's name and the shell's version.
///
/// While the window is open its rectangle joins the frame's overlay regions,
/// so a click on it is the UI's and never a scene pick.
pub fn about_window(
    context: &egui::Context,
    mark: &egui::TextureHandle,
    open: &mut bool,
    overlay_regions: &mut Vec<ShellRect>,
) {
    if !*open {
        return;
    }
    // Step 1: Draw the window.
    let mut opened = *open;
    let window = egui::Window::new("About Pill Editor")
        .open(&mut opened)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(context, |ui| {
            ui.horizontal(|ui| {
                let mark_size = egui::vec2(40.0, 40.0);
                ui.add(egui::Image::new((mark.id(), mark_size)));
                ui.vertical(|ui| {
                    ui.heading("Pill Editor");
                    ui.label(format!("shell {}", env!("CARGO_PKG_VERSION")));
                    ui.label("an egui frontend for the Pill engine");
                });
            });
        });
    // Step 2: Record the window's rectangle as an overlay region, and keep
    // the open flag egui reports back.
    if let Some(window) = window {
        claim_pointer(window.response.rect, overlay_regions);
    }
    *open = opened;
}
