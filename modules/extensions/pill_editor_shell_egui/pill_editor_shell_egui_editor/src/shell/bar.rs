//! The menu bar: the rows' menus and the actions they run.
//!
//! # Responsibilities
//!
//! - The bar's menus and their items, drawn from the shell's mark and the project's menu state.
//! - The actions a menu item runs (`File/Quit`, the window toggles, the gizmo rows), each queueing the same recorded command the shortcuts queue.
//! - The About window the Help menu opens.
//!
//! Bodies unchanged from the shell's root file (W-0037 Stage 3); the globs
//! below are absolute paths, so a patch crate resolves them too.

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;
use pill_editor_shell_egui_editor::shell::*;

/// The gap between two menu titles in the bar.
pub const MENU_TITLE_GAP: f32 = 2.0;

/// The menu bar above the dock: the pill mark, then Unity's six titles.
///
/// Drawn as a top panel before the dock, so the dock's layout and every
/// viewport rectangle the frame reports start below the strip. The bar's
/// geometry is logged once per placement (the toolbox's pattern), so a suite
/// can click a title without guessing at fonts or theme metrics, and an open
/// menu's entries log theirs for the same reason.
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
    let tint = style.visuals.text_color();
    egui::Panel::top("pill_editor_shell_egui.menu_bar")
        .exact_size(menubar::MENU_BAR_HEIGHT)
        .frame(
            egui::Frame::side_top_panel(style.as_ref()).inner_margin(egui::Margin::symmetric(0, 4)),
        )
        .show(ui, |ui| {
            let bar = ui.max_rect();
            let mut open_menu: Option<menubar::Menu> = None;
            if session.bar != Some(bar) {
                session.bar = Some(bar);
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ENGINE,
                    x = bar.min.x,
                    y = bar.min.y,
                    width = bar.width(),
                    height = bar.height(),
                    "[editor] menu bar rect"
                );
            }
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                // Unity's strip: plain labels at rest, a wash under the
                // pointer or the open menu - not resting buttons.
                ui.visuals_mut().widgets.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
                ui.add_space(menubar::MENU_MARK_LEFT);
                let side = egui::vec2(menubar::MENU_MARK_SIDE, menubar::MENU_MARK_SIDE);
                ui.add(egui::Image::new((mark.id(), side)).tint(tint));
                ui.add_space(menubar::MENU_MARK_GAP);
                for (index, menu) in menubar::Menu::ALL.iter().enumerate() {
                    if index > 0 {
                        ui.add_space(MENU_TITLE_GAP);
                    }
                    let title = ui.menu_button(menu.title(), |ui| {
                        menu_body(ui, *menu, session, viewer);
                    });
                    if title.inner.is_some() {
                        open_menu = Some(*menu);
                    }
                    let response = title.response;
                    if session.titles[index] != Some(response.rect) {
                        session.titles[index] = Some(response.rect);
                        pill_core::info!(
                            target: pill_core::telemetry::telemetry_target::ENGINE,
                            name = menu.name(),
                            x = response.rect.min.x,
                            y = response.rect.min.y,
                            width = response.rect.width(),
                            height = response.rect.height(),
                            "[editor] menu item rect"
                        );
                    }
                }
            });
            if let Some(menu) = open_menu {
                if session.open != Some(menu) {
                    session.open = Some(menu);
                    pill_core::info!(
                        target: pill_core::telemetry::telemetry_target::ENGINE,
                        menu = menu.name(),
                        "[editor] menu opened"
                    );
                }
            } else {
                session.open = None;
            }
        });
}

/// One open menu: its model rows, their placement logging, and the action a
/// click runs.
pub fn menu_body(
    ui: &mut egui::Ui,
    menu: menubar::Menu,
    session: &mut MenubarSession,
    viewer: &mut EditorTabViewer<'_>,
) {
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
                let response = ui.add_enabled(
                    enabled,
                    egui::Button::selectable(checked.unwrap_or(false), label.as_str()),
                );
                let key = format!("{}/{}", menu.name(), label);
                if session
                    .entries
                    .iter()
                    .find(|(logged, _)| *logged == key)
                    .is_none_or(|(_, rect)| *rect != response.rect)
                {
                    if let Some((_, rect)) = session
                        .entries
                        .iter_mut()
                        .find(|(logged, _)| *logged == key)
                    {
                        *rect = response.rect;
                    } else {
                        session.entries.push((key, response.rect));
                    }
                    pill_core::info!(
                        target: pill_core::telemetry::telemetry_target::ENGINE,
                        menu = menu.name(),
                        label,
                        x = response.rect.min.x,
                        y = response.rect.min.y,
                        width = response.rect.width(),
                        height = response.rect.height(),
                        "[editor] menu entry rect"
                    );
                }
                if response.clicked() {
                    run_menu_action(menu, label.as_str(), action, session, viewer);
                    ui.close();
                }
            }
        }
    }
    // The popup floats over the dock and the viewports, so its rectangle is
    // the UI's: a click on an item must never become a scene pick.
    let rect = ui.min_rect();
    viewer.overlay_regions.push(ShellRect {
        x: rect.min.x,
        y: rect.min.y,
        width: rect.width(),
        height: rect.height(),
    });
}

/// Run one menu item through the same calls the tabs, the toolbox and the
/// scripted channel make.
pub fn run_menu_action(
    menu: menubar::Menu,
    label: &str,
    action: menubar::MenuAction,
    session: &mut MenubarSession,
    viewer: &mut EditorTabViewer<'_>,
) {
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
        menubar::MenuAction::WriteAssetMeta => {
            assets::write_meta_selection(viewer.pending, viewer.selection, &viewer.snapshot.assets);
        }
        menubar::MenuAction::Undo => session
            .history_requests
            .push(pill_editor_core_egui::session::HistoryRequest::Undo),
        menubar::MenuAction::Redo => session
            .history_requests
            .push(pill_editor_core_egui::session::HistoryRequest::Redo),
        menubar::MenuAction::ActivateTab(tab) => session.pending_tab = Some(tab),
        menubar::MenuAction::About => session.about_open = true,
        menubar::MenuAction::Quit => session.quit_requested = true,
    }
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ENGINE,
        menu = menu.name(),
        label,
        "[editor] menu action"
    );
}

/// The About window: the mark, the editor's name and the shell's version.
pub fn about_window(context: &egui::Context, mark: &egui::TextureHandle, open: &mut bool) {
    if !*open {
        return;
    }
    let mut opened = *open;
    egui::Window::new("About Pill Editor")
        .open(&mut opened)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(context, |ui| {
            ui.horizontal(|ui| {
                let side = egui::vec2(40.0, 40.0);
                ui.add(egui::Image::new((mark.id(), side)));
                ui.vertical(|ui| {
                    ui.heading("Pill Editor");
                    ui.label(format!("shell {}", env!("CARGO_PKG_VERSION")));
                    ui.label("an egui frontend for the Pill engine");
                });
            });
        });
    *open = opened;
}
