//! The Inspector's asset import settings.
//!
//! # Responsibilities
//!
//! - `asset_settings_rows` and `asset_settings_row`: the selected asset's settings as a recursive row walk.
//! - `settings_commit`: the recorded command one committed settings value queues.
//!
//! Bodies unchanged from the shell's root file (W-0037 Stage 3); the globs
//! below are absolute paths, so a patch crate resolves them too.

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;
use pill_editor_shell_egui_editor::shell::*;

/// The settings document's rows: objects and arrays nest their children,
/// every scalar leaf edits.
pub fn asset_settings_rows(
    ui: &mut egui::Ui,
    asset_path: &str,
    settings: &JsonValue,
    pending: &mut PendingEditorCommands,
) {
    match settings {
        JsonValue::Object(entries) => {
            for (key, value) in entries {
                asset_settings_row(ui, asset_path, settings, value, key.clone(), pending);
            }
        }
        other => {
            read_only_row(ui, "settings", &other.to_string());
        }
    }
}

/// One settings row: a leaf gets the widget its JSON kind asks for (its
/// commit carries the whole patched document), a container nests.
pub fn asset_settings_row(
    ui: &mut egui::Ui,
    asset_path: &str,
    root: &JsonValue,
    value: &JsonValue,
    key: String,
    pending: &mut PendingEditorCommands,
) {
    match value {
        JsonValue::Object(entries) => {
            ui.label(egui::RichText::new(key.as_str()).color(style::TEXT_MUTED));
            ui.indent(key.as_str(), |ui| {
                for (child_key, child) in entries {
                    asset_settings_row(
                        ui,
                        asset_path,
                        root,
                        child,
                        format!("{key}.{child_key}"),
                        pending,
                    );
                }
            });
        }
        JsonValue::Array(elements) => {
            ui.label(egui::RichText::new(key.as_str()).color(style::TEXT_MUTED));
            ui.indent(key.as_str(), |ui| {
                for (index, child) in elements.iter().enumerate() {
                    asset_settings_row(
                        ui,
                        asset_path,
                        root,
                        child,
                        format!("{key}[{index}]"),
                        pending,
                    );
                }
            });
        }
        JsonValue::Bool(current) => {
            let mut edited = *current;
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new(key.as_str()).color(style::TEXT_MUTED));
                if ui.checkbox(&mut edited, "").changed() {
                    settings_commit(pending, asset_path, root, &key, JsonValue::Bool(edited));
                }
            });
        }
        JsonValue::Number(current) => {
            let mut integer_edit = current
                .as_i64()
                .or_else(|| current.as_u64().map(|value| value as i64));
            match integer_edit.as_mut() {
                Some(edited) => {
                    let mut changed = false;
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new(key.as_str()).color(style::TEXT_MUTED));
                        changed = ui.add(egui::DragValue::new(edited)).changed();
                    });
                    if changed {
                        settings_commit(
                            pending,
                            asset_path,
                            root,
                            &key,
                            JsonValue::Number((*edited).into()),
                        );
                    }
                }
                None => {
                    let mut edited = current.as_f64().unwrap_or_default();
                    let mut changed = false;
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new(key.as_str()).color(style::TEXT_MUTED));
                        changed = ui
                            .add(
                                egui::DragValue::new(&mut edited)
                                    .speed(0.05)
                                    .max_decimals(9),
                            )
                            .changed();
                    });
                    if changed {
                        if let Some(number) = serde_json::Number::from_f64(edited) {
                            settings_commit(
                                pending,
                                asset_path,
                                root,
                                &key,
                                JsonValue::Number(number),
                            );
                        }
                    }
                }
            }
        }
        JsonValue::String(current) => {
            let mut edited = current.clone();
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new(key.as_str()).color(style::TEXT_MUTED));
                let response = ui.add(egui::TextEdit::singleline(&mut edited).desired_width(160.0));
                if response.lost_focus() && edited != *current {
                    settings_commit(pending, asset_path, root, &key, JsonValue::String(edited));
                }
            });
        }
        other => {
            read_only_row(ui, key.as_str(), &other.to_string());
        }
    }
}

/// Queue the recorded edit that puts `value` at `key` in the settings; the
/// command carries the whole patched document, the native contract.
pub fn settings_commit(
    pending: &mut PendingEditorCommands,
    asset_path: &str,
    root: &JsonValue,
    key: &str,
    value: JsonValue,
) {
    let Ok(path) = inspector::parse_settings_key_path(key) else {
        return;
    };
    let mut document = root.clone();
    if let Err(message) = inspector::patch_settings(&mut document, &path, value) {
        pill_core::warn!(
            target: pill_core::telemetry::telemetry_target::ECS,
            key,
            message = message.as_str(),
            "[editor] asset settings edit refused"
        );
        return;
    }
    let (command, label) = inspector::settings_edit(asset_path, document, key);
    pending.push_recorded(
        vec![command],
        label,
        Some(inspector::settings_coalesce_key(asset_path, key)),
    );
}
