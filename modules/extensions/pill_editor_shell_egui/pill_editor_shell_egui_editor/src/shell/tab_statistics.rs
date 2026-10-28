//! The Statistics tab: what one frame costs, and what each tab costs.
//!
//! # Responsibilities
//!
//! - Show the frame rate, the whole shell pass's time, the world's size and
//!   one row per tab drawn since the session began, with its smoothed body
//!   time.
//!
//! # Design
//!
//! Every number here is a measurement the shell already took: the whole
//! pass's time in `run_frame` and each `TabViewer::ui` body's time. The tab
//! only formats them; it records nothing.

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;
use pill_editor_shell_egui_editor::shell::*;

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
    value_row_in(
        ui,
        WIDE_ROW_LABEL_WIDTH,
        "frames per second",
        &format!("{frames_per_second:.1}"),
    );
    value_row_in(
        ui,
        WIDE_ROW_LABEL_WIDTH,
        "frame time",
        &format!("{frame_micros:.0} us"),
    );
    value_row_in(
        ui,
        WIDE_ROW_LABEL_WIDTH,
        "shell pass",
        &format!(
            "{:.0} us ({} passes)",
            stats.shell_micros, stats.shell_passes
        ),
    );
    value_row_in(
        ui,
        WIDE_ROW_LABEL_WIDTH,
        "entities",
        &snapshot.entities.len().to_string(),
    );
    value_row_in(
        ui,
        WIDE_ROW_LABEL_WIDTH,
        "systems",
        &snapshot.systems.len().to_string(),
    );
    value_row_in(
        ui,
        WIDE_ROW_LABEL_WIDTH,
        "console lines",
        &snapshot.console.total.to_string(),
    );
    value_row_in(
        ui,
        WIDE_ROW_LABEL_WIDTH,
        "problems",
        &snapshot.problems.len().to_string(),
    );
    ui.separator();
    if stats.tabs.is_empty() {
        ui.weak("No tab measured yet");
        return;
    }
    ui.label(egui::RichText::new("per-tab body time").strong());
    for entry in &stats.tabs {
        row_in(
            ui,
            WIDE_ROW_LABEL_WIDTH,
            &entry.tab.title(),
            None,
            &mut |ui| {
                ui.monospace(format!("{:.0} us", entry.micros));
                ui.weak(format!("{} passes", entry.passes));
            },
        );
    }
}
