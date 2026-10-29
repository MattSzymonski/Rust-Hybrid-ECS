//! The Console tab: the engine's and the project's recent log lines.
//!
//! # Responsibilities
//!
//! - Draw the filter row - the level buttons, the channel switch and the
//!   search field - and the rows the filters leave.
//! - Edit the filters in the shared session; the executable applies them to
//!   the history on the next frame, so the tab never touches the world.
//! - Say how much history the view is holding back.
//!
//! # Design
//!
//! The rows come from `ConsoleView` in the frame's snapshot: the newest
//! `CONSOLE_VIEW_ROWS` lines the filters leave, already owned strings. The
//! tab is therefore a pure function of the snapshot plus the session's filter
//! fields, which is what makes it cheap to draw every frame.

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;
// Deliberately unused in this compilation: a patch crate replays this
// file's opening, and the first sibling this body names resolves through it.
#[allow(unused_imports)]
use pill_editor_shell_egui_editor::shell::*;

/// The levels the filter row offers, lowest first.
pub const CONSOLE_LEVELS: [(u8, &str); 5] = [
    (1, "trace"),
    (2, "debug"),
    (3, "info"),
    (4, "warn"),
    (5, "error"),
];

/// The Console tab: filters above, the newest rows below.
pub fn console_tab(
    ui: &mut egui::Ui,
    snapshot: &EditorSnapshot,
    filters: &mut ConsoleFilterSettings,
) {
    let view = &snapshot.console;
    ui.horizontal_wrapped(|ui| {
        ui.label("Level");
        for (level, name) in CONSOLE_LEVELS {
            let selected = filters.minimum_level == level;
            if ui
                .add(egui::Button::selectable(selected, name))
                .on_hover_text(format!("show {name} and above"))
                .clicked()
            {
                filters.minimum_level = level;
            }
        }
        ui.separator();
        ui.label("Channel");
        for (name, channel) in [
            ("both", None),
            ("engine", Some(ConsoleChannel::Engine)),
            ("game", Some(ConsoleChannel::Game)),
        ] {
            let selected = filters.channel == channel;
            if ui.add(egui::Button::selectable(selected, name)).clicked() {
                filters.channel = channel;
            }
        }
        ui.separator();
        ui.label("Search");
        ui.add(
            egui::TextEdit::singleline(&mut filters.search)
                .hint_text("substring")
                .desired_width(160.0),
        );
    });
    ui.separator();
    // The counts read as `visible / total`: the history drops its oldest
    // lines at the cap, and the filter row says how much it is hiding.
    ui.label(
        egui::RichText::new(format!(
            "{} of {} lines (level {}+, channel {}, search {})",
            view.visible,
            view.total,
            console::level_name(filters.minimum_level),
            filters.channel.map_or("both", ConsoleChannel::name),
            if filters.search.trim().is_empty() {
                "-"
            } else {
                filters.search.as_str()
            }
        ))
        .small()
        .color(style::text()),
    );
    egui::ScrollArea::vertical()
        .stick_to_bottom(true)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for line in &view.lines {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(line.level_name())
                            .monospace()
                            .color(console_level_colour(line.level)),
                    );
                    ui.label(
                        egui::RichText::new(format!("{}: {}", line.target, line.message))
                            .monospace(),
                    );
                });
            }
            if view.lines.is_empty() {
                ui.weak("No lines match");
            }
        });
}

/// The colour of a level's tag: the toast stack's roles, so one level reads
/// the same everywhere.
pub fn console_level_colour(level: u8) -> egui::Color32 {
    match level {
        4 => style::log_warn(),
        5 => style::log_error(),
        _ => style::log_info(),
    }
}
