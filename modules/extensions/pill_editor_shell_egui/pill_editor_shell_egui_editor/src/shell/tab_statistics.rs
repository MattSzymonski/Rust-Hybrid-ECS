//! The Statistics tab: what one frame costs, and what each tab costs.
//!
//! # Responsibilities
//!
//! - Show the frame rate, the whole shell pass's time, the world's size and
//!   one row per tab that has been drawn with its smoothed body time
//!   (W-0038 Stage 7).
//!
//! # Design
//!
//! Every number here is a measurement the shell already took: the pass time
//! around `run_ui` and the per-tab times around each `TabViewer::ui` body.
//! The tab only formats them, so reading it never changes what it reports.

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;

/// The Statistics tab: the frame's numbers, then one row per drawn tab.
pub fn statistics_tab(
    ui: &mut egui::Ui,
    snapshot: &EditorSnapshot,
    stats: &StatisticsSession,
    frames_per_second: f32,
) {
    let frame_micros = if frames_per_second > 0.0 {
        1_000_000.0 / frames_per_second as f64
    } else {
        0.0
    };
    egui::Grid::new("pill_editor_shell_egui.statistics")
        .num_columns(2)
        .spacing([18.0, 4.0])
        .show(ui, |ui| {
            row(ui, "frames per second", &format!("{frames_per_second:.1}"));
            row(ui, "frame time", &format!("{frame_micros:.0} us"));
            row(
                ui,
                "shell pass",
                &format!(
                    "{:.0} us ({} passes)",
                    stats.shell_micros, stats.shell_passes
                ),
            );
            row(ui, "entities", &snapshot.entities.len().to_string());
            row(ui, "systems", &snapshot.systems.len().to_string());
            row(ui, "console lines", &snapshot.console.total.to_string());
            row(ui, "problems", &snapshot.problems.len().to_string());
        });
    ui.separator();
    if stats.tabs.is_empty() {
        ui.weak("No tab measured yet");
        return;
    }
    ui.label(egui::RichText::new("per-tab body time").strong());
    egui::Grid::new("pill_editor_shell_egui.statistics.tabs")
        .num_columns(3)
        .spacing([18.0, 2.0])
        .show(ui, |ui| {
            for entry in &stats.tabs {
                ui.label(entry.tab.title());
                ui.label(format!("{:.0} us", entry.micros));
                ui.label(format!("{} passes", entry.passes));
                ui.end_row();
            }
        });
}

/// One label/value pair of the top grid.
///
/// Public because the build script's inventory names every function by its
/// module path: a private one would not be patchable.
pub fn row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.label(label);
    ui.label(egui::RichText::new(value).monospace());
    ui.end_row();
}
