//! The command palette: a searchable popup over every registered command.
//!
//! # Responsibilities
//!
//! - Draw the palette while it is open: the query field, the filtered rows and
//!   the row the arrows select (W-0038 Stage 3).
//! - Run the selected row through [`run_menu_action`], the same call a menu
//!   entry makes, so a palette run and a menu click take one path.
//! - Report the popup's rectangle as an overlay region, so a click inside the
//!   palette is never a scene pick.
//!
//! The model lives in `PaletteState` (the shared session): the scripted
//! channel sets a query, a run request or a close on it, and this file only
//! turns that state into pixels and actions. That is why the suite needs no
//! synthetic keystrokes to drive it.

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;
use pill_editor_shell_egui_editor::shell::*;

/// How many rows the popup lists before the list is cut off.
pub const PALETTE_ROWS: usize = 12;

/// Where the popup's top centre sits, below the menu bar.
pub const PALETTE_TOP: f32 = 72.0;

/// How wide the popup is, in logical points.
pub const PALETTE_WIDTH: f32 = 360.0;

/// The commands whose titles contain `query`, in registration order.
///
/// A substring match on the title, case-insensitively; fuzzy ranking is out
/// of scope for this stage.
pub fn matches<'a>(commands: &'a [CommandView], query: &str) -> Vec<&'a CommandView> {
    let needle = query.trim().to_lowercase();
    commands
        .iter()
        .filter(|command| needle.is_empty() || command.title.to_lowercase().contains(&needle))
        .collect()
}

/// Draw the palette when it is open and run the row it was asked to run.
pub fn palette_window(
    context: &egui::Context,
    palette: &mut PaletteState,
    session: &mut MenubarSession,
    viewer: &mut EditorTabViewer<'_>,
) {
    if !palette.open {
        return;
    }
    let rows: Vec<CommandView> = matches(&viewer.snapshot.commands, &palette.query)
        .into_iter()
        .cloned()
        .collect();
    palette.selected = palette.selected.min(rows.len().saturating_sub(1));
    // One line per query change, so a suite can wait for the filter result
    // instead of reading the screen.
    if palette.logged_query.as_deref() != Some(palette.query.as_str()) {
        palette.logged_query = Some(palette.query.clone());
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ENGINE,
            query = palette.query.as_str(),
            matches = rows.len(),
            first = rows.first().map_or("", |row| row.title.as_str()),
            "[editor] palette query"
        );
    }
    let mut clicked = None;
    let window = egui::Window::new("Command Palette")
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, PALETTE_TOP))
        .default_width(PALETTE_WIDTH)
        .show(context, |ui| {
            ui.set_width(PALETTE_WIDTH);
            let field = ui.add(
                egui::TextEdit::singleline(&mut palette.query)
                    .hint_text("Search commands")
                    .desired_width(f32::INFINITY),
            );
            if !field.has_focus() {
                field.request_focus();
            }
            // Arrows move the selection, Escape closes; Enter sets the same
            // run request a click does.
            let (down, up, enter, escape) = ui.input(|input| {
                (
                    input.key_pressed(egui::Key::ArrowDown),
                    input.key_pressed(egui::Key::ArrowUp),
                    input.key_pressed(egui::Key::Enter),
                    input.key_pressed(egui::Key::Escape),
                )
            });
            if down && !rows.is_empty() {
                palette.selected = (palette.selected + 1) % rows.len();
            }
            if up && !rows.is_empty() {
                palette.selected = (palette.selected + rows.len() - 1) % rows.len();
            }
            if enter {
                palette.run_requested = true;
            }
            if escape {
                palette.open = false;
                palette.run_requested = false;
            }
            ui.separator();
            for (index, row) in rows.iter().take(PALETTE_ROWS).enumerate() {
                let label = format!("{}  -  {}", row.title, row.id);
                if ui
                    .add(egui::Button::selectable(index == palette.selected, label))
                    .clicked()
                {
                    clicked = Some(index);
                }
            }
            if rows.is_empty() {
                ui.weak("No matching command");
            }
        });
    if let Some(index) = clicked {
        palette.selected = index;
        palette.run_requested = true;
    }
    if let Some(response) = window {
        let rect = response.response.rect;
        viewer.overlay_regions.push(ShellRect {
            x: rect.min.x,
            y: rect.min.y,
            width: rect.width(),
            height: rect.height(),
        });
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ENGINE,
            x = rect.min.x,
            y = rect.min.y,
            width = rect.width(),
            height = rect.height(),
            "[editor] palette rect"
        );
    }
    if std::mem::take(&mut palette.run_requested) {
        run_selected(palette, session, viewer, &rows);
    }
}

/// Run the selected row, if one matched, and close the palette.
pub fn run_selected(
    palette: &mut PaletteState,
    session: &mut MenubarSession,
    viewer: &mut EditorTabViewer<'_>,
    rows: &[CommandView],
) {
    let Some(row) = rows.get(palette.selected) else {
        palette.open = false;
        return;
    };
    let label = row.title.clone();
    let action = menubar::action_for(row, viewer.snapshot, viewer.selection);
    palette.open = false;
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ENGINE,
        command = row.id.as_str(),
        "[editor] palette closed"
    );
    run_menu_action("palette", label.as_str(), action, session, viewer);
}
