//! The Problems tab: build errors and warnings, grouped by module.
//!
//! # Responsibilities
//!
//! - Draw one row per diagnostic the executable collected from a module build
//!   or a patch, grouped under the module that reported it (W-0038 Stage 6).
//! - Turn a row click into an open request for the executable, which knows the
//!   `code_editor_command` preference and may spawn a process.
//!
//! # Design
//!
//! The rows arrive in the frame's snapshot (`EditorSnapshot.problems`), which
//! the executable fills from the same notices the build-failed toasts use, so
//! a row and its toast can never disagree. The tab is read-only apart from the
//! request it leaves in the session.

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;

/// The Problems tab: the diagnostics grouped by module, newest module first.
pub fn problems_tab(ui: &mut egui::Ui, snapshot: &EditorSnapshot, request: &mut Option<usize>) {
    if snapshot.problems.is_empty() {
        ui.weak("No problems");
        return;
    }
    // The groups keep the order the executable reported them in: a module
    // that just failed sits where its build finished.
    let mut groups: Vec<(&str, Vec<usize>)> = Vec::new();
    for (index, row) in snapshot.problems.iter().enumerate() {
        match groups.iter_mut().find(|(module, _)| *module == row.module) {
            Some((_, indices)) => indices.push(index),
            None => groups.push((row.module.as_str(), vec![index])),
        }
    }
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for (module, indices) in groups {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!("{module} ({})", indices.len()))
                            .strong()
                            .color(style::TEXT),
                    );
                });
                for index in indices {
                    let row = &snapshot.problems[index];
                    // `<file>:<line>` is the clickable part; a diagnostic
                    // without a location still reads, it just cannot open.
                    let location = match (&row.problem.file, row.problem.line) {
                        (Some(file), Some(line)) => format!("{file}:{line}"),
                        (Some(file), None) => file.clone(),
                        _ => "-".to_string(),
                    };
                    let label = format!(
                        "  {}  {location}  {}",
                        row.problem.severity.name(),
                        row.problem.message
                    );
                    let response =
                        ui.add(egui::Button::new(egui::RichText::new(label).monospace()).wrap());
                    if response.clicked() && row.problem.file.is_some() {
                        *request = Some(index);
                    }
                }
                ui.separator();
            }
        });
}
