//! Assets panel: the project's `res` tree, an asset's settings, and moving
//! assets.
//!
//! # Responsibilities
//!
//! - List the project's `res` tree with each file's asset type, load state and
//!   guid (`.meta` files hidden).
//! - Edit the selected asset's import settings - or a standalone asset's own
//!   document - as a generic form over its JSON, and save it.
//! - Rename or move the selected asset together with its `.meta`.
//! - Create a new standalone asset (a material, a render pass) from a dialog:
//!   a type dropdown filled from the registry, a name, and a folder chosen
//!   from the `res` tree, opened from a button or a folder's context menu.
//!
//! # Design
//!
//! The panel knows no asset type. Rows and settings come from the host's asset
//! browser, which reads the import registry, and the form is built from the
//! settings' JSON value: a checkbox for a boolean, a text field for a number or
//! a string, and a JSON text box for anything nested. A type a module adds
//! later is listed and edited with no change here.
//!
//! Saving writes the file and nothing else. The dev host's asset watcher
//! reimports the asset in place at the next frame, as it would for an edit in
//! any other program, so the running scene updates without the editor being a
//! special case.

use std::sync::Arc;
use std::time::Duration;

use dioxus::prelude::*;
use pill_host::{standalone_file_name, AssetEntry, StandaloneType};

use crate::EditorContext;

/// How often the tree is re-read from disk.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How one settings field is edited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SettingKind {
    /// A JSON boolean, edited with a checkbox.
    Bool,
    /// A JSON number, edited as text and parsed back.
    Number,
    /// A JSON string, edited as text.
    Text,
    /// Anything else (an object, an array, `null`), edited as JSON text.
    Json,
}

/// One top-level settings field and its text as edited.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SettingField {
    /// The field's key in the settings object.
    pub(crate) key: String,
    /// How the field is edited.
    pub(crate) kind: SettingKind,
    /// The value as text: `true`/`false`, the number, the string, or JSON.
    pub(crate) text: String,
}

/// The form fields for a settings value: one per top-level key of an object,
/// in key order. A value that is not an object is one JSON field named `value`.
pub(crate) fn fields_from_settings(settings: &serde_json::Value) -> Vec<SettingField> {
    let Some(object) = settings.as_object() else {
        return vec![SettingField {
            key: "value".to_owned(),
            kind: SettingKind::Json,
            text: settings.to_string(),
        }];
    };
    object
        .iter()
        .map(|(key, value)| {
            let (kind, text) = match value {
                serde_json::Value::Bool(flag) => (SettingKind::Bool, flag.to_string()),
                serde_json::Value::Number(number) => (SettingKind::Number, number.to_string()),
                serde_json::Value::String(text) => (SettingKind::Text, text.clone()),
                other => (
                    SettingKind::Json,
                    serde_json::to_string_pretty(other).unwrap_or_default(),
                ),
            };
            SettingField {
                key: key.clone(),
                kind,
                text,
            }
        })
        .collect()
}

/// The settings value the edited fields describe.
///
/// # Errors
///
/// A message naming the field when a number or a JSON field does not parse.
pub(crate) fn settings_from_fields(fields: &[SettingField]) -> Result<serde_json::Value, String> {
    if let [only] = fields {
        if only.key == "value" && only.kind == SettingKind::Json {
            return serde_json::from_str(&only.text).map_err(|error| format!("value: {error}"));
        }
    }
    let mut object = serde_json::Map::new();
    for field in fields {
        let value = match field.kind {
            SettingKind::Bool => serde_json::Value::Bool(field.text == "true"),
            SettingKind::Text => serde_json::Value::String(field.text.clone()),
            SettingKind::Number => {
                let text = field.text.trim();
                let number = text
                    .parse::<i64>()
                    .map(serde_json::Number::from)
                    .ok()
                    .or_else(|| {
                        text.parse::<f64>()
                            .ok()
                            .and_then(serde_json::Number::from_f64)
                    })
                    .ok_or_else(|| format!("{}: `{text}` is not a number", field.key))?;
                serde_json::Value::Number(number)
            }
            SettingKind::Json => serde_json::from_str(&field.text)
                .map_err(|error| format!("{}: {error}", field.key))?,
        };
        object.insert(field.key.clone(), value);
    }
    Ok(serde_json::Value::Object(object))
}

/// The last `::` segment of a type name, for a compact row subtitle.
pub(crate) fn short_type_name(type_name: &str) -> &str {
    type_name.rsplit("::").next().unwrap_or(type_name)
}

/// One row's subtitle: type, load state and guid, or why it is not an asset.
fn row_subtitle(entry: &AssetEntry) -> String {
    if entry.is_directory {
        return "folder".to_owned();
    }
    let Some(type_name) = &entry.type_name else {
        return "not an asset type".to_owned();
    };
    let state = if entry.loaded { "loaded" } else { "not loaded" };
    let guid = entry.guid.as_deref().map_or("no guid yet", |guid| guid);
    format!("{} · {state} · {guid}", short_type_name(type_name))
}

/// The folder a new asset goes into by default: the selected folder, the
/// selected file's folder, or `res` itself (`""`).
pub(crate) fn folder_for_new_asset(selected: Option<&AssetEntry>) -> String {
    match selected {
        Some(entry) if entry.is_directory => entry.path.clone(),
        Some(entry) => entry
            .path
            .rsplit_once('/')
            .map(|(folder, _)| folder.to_owned())
            .unwrap_or_default(),
        None => String::new(),
    }
}

/// The path a new asset would get, shown before it is created, by the host's
/// own naming rule; or why the name is refused.
pub(crate) fn preview_path(folder: &str, name: &str, extension: &str) -> Result<String, String> {
    let file_name = standalone_file_name(name, extension)?;
    Ok(if folder.is_empty() {
        format!("res/{file_name}")
    } else {
        format!("res/{folder}/{file_name}")
    })
}

/// The Assets panel body.
#[component]
pub(crate) fn AssetsTab(editor: Arc<EditorContext>) -> Element {
    let mut entries = use_signal(Vec::<AssetEntry>::new);
    let mut selected = use_signal(|| Option::<String>::None);
    let mut fields = use_signal(Vec::<SettingField>::new);
    let mut move_target = use_signal(String::new);
    let mut status = use_signal(String::new);
    // The Create asset dialog: open or not, the types it offers, and its fields.
    let mut create_open = use_signal(|| false);
    let mut create_types = use_signal(Vec::<StandaloneType>::new);
    let mut create_type = use_signal(String::new);
    let mut create_name = use_signal(String::new);
    let mut create_folder = use_signal(String::new);
    let mut create_error = use_signal(String::new);

    let poll_editor = Arc::clone(&editor);
    use_future(move || {
        let poll_editor = Arc::clone(&poll_editor);
        async move {
            loop {
                entries.set(poll_editor.asset_entries());
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        }
    });

    let rows = entries.read().clone();
    let selected_path = selected.read().clone();
    let selected_entry = selected_path
        .as_ref()
        .and_then(|path| rows.iter().find(|entry| &entry.path == path).cloned());
    let folders: Vec<AssetEntry> = rows
        .iter()
        .filter(|entry| entry.is_directory)
        .cloned()
        .collect();

    // Opens the dialog with `folder` chosen and the type list read fresh, so a
    // standalone type a module registered since is offered at once.
    let open_editor = Arc::clone(&editor);
    let open_create = move |folder: String| {
        let types = open_editor.standalone_asset_types();
        if !types
            .iter()
            .any(|kind| kind.type_name == *create_type.read())
        {
            create_type.set(
                types
                    .first()
                    .map(|kind| kind.type_name.clone())
                    .unwrap_or_default(),
            );
        }
        create_types.set(types);
        create_folder.set(folder);
        create_error.set(String::new());
        create_open.set(true);
    };
    let chosen_extension = create_types
        .read()
        .iter()
        .find(|kind| kind.type_name == *create_type.read())
        .map(|kind| kind.extension.clone())
        .unwrap_or_default();
    let preview = preview_path(
        &create_folder.read(),
        &create_name.read(),
        &chosen_extension,
    );

    rsx! {
        div {
            class: "editor-panel editor-assets",
            div {
                class: "editor-panel-toolbar",
                button {
                    onclick: {
                        let selected_entry = selected_entry.clone();
                        let mut open_create = open_create.clone();
                        move |_| open_create(folder_for_new_asset(selected_entry.as_ref()))
                    },
                    "Create asset…"
                }
            }
            if *create_open.read() {
                div {
                    class: "editor-create-dialog",
                    div { class: "editor-row-title", "Create asset" }
                    div {
                        class: "editor-field",
                        label { "Type" }
                        select {
                            value: "{create_type}",
                            onchange: move |event| create_type.set(event.value()),
                            for kind in create_types.read().clone() {
                                option { value: "{kind.type_name}", "{kind.display_name}" }
                            }
                        }
                    }
                    div {
                        class: "editor-field",
                        label { "Name" }
                        input {
                            r#type: "text",
                            value: "{create_name}",
                            oninput: move |event| create_name.set(event.value()),
                        }
                    }
                    div { class: "editor-row-subtitle", "Folder" }
                    ul {
                        class: "editor-list editor-folder-picker",
                        li {
                            key: "folder-root",
                            class: if create_folder.read().is_empty() { "editor-list-row editor-selected" } else { "editor-list-row" },
                            onclick: move |_| create_folder.set(String::new()),
                            "res"
                        }
                        for folder in folders {
                            li {
                                key: "folder-{folder.path}",
                                class: if *create_folder.read() == folder.path { "editor-list-row editor-selected" } else { "editor-list-row" },
                                style: "padding-left: {folder.depth * 14 + 18}px",
                                onclick: {
                                    let path = folder.path.clone();
                                    move |_| create_folder.set(path.clone())
                                },
                                "{folder.name}"
                            }
                        }
                    }
                    match &preview {
                        Ok(path) => rsx! { div { class: "editor-row-subtitle", "Creates {path}" } },
                        Err(reason) => rsx! { div { class: "editor-row-subtitle editor-warn", "{reason}" } },
                    }
                    if !create_error.read().is_empty() {
                        div { class: "editor-row-subtitle editor-warn", "{create_error}" }
                    }
                    div {
                        class: "editor-panel-toolbar",
                        button {
                            disabled: preview.is_err() || create_type.read().is_empty(),
                            onclick: {
                                let editor = Arc::clone(&editor);
                                move |_| {
                                    let result = editor.create_standalone_asset(
                                        &create_type.read(),
                                        &create_folder.read(),
                                        &create_name.read(),
                                    );
                                    match result {
                                        Ok(path) => {
                                            create_open.set(false);
                                            create_name.set(String::new());
                                            selected.set(Some(path.clone()));
                                            status.set(format!("created res/{path}; it is loaded"));
                                        }
                                        Err(error) => create_error.set(error),
                                    }
                                }
                            },
                            "Create"
                        }
                        button { onclick: move |_| create_open.set(false), "Cancel" }
                    }
                }
            }
            ul {
                class: "editor-list",
                for entry in rows {
                    li {
                        key: "asset-{entry.path}",
                        class: if selected_path.as_deref() == Some(entry.path.as_str()) {
                            "editor-list-row editor-selected"
                        } else {
                            "editor-list-row"
                        },
                        style: "padding-left: {entry.depth * 14 + 4}px",
                        // A folder's context menu opens the Create dialog there.
                        oncontextmenu: {
                            let entry = entry.clone();
                            let mut open_create = open_create.clone();
                            move |event: Event<MouseData>| {
                                if entry.is_directory {
                                    event.prevent_default();
                                    open_create(entry.path.clone());
                                }
                            }
                        },
                        onclick: {
                            let editor = Arc::clone(&editor);
                            let entry = entry.clone();
                            move |_| {
                                if entry.is_directory || entry.type_name.is_none() {
                                    selected.set(Some(entry.path.clone()));
                                    fields.set(Vec::new());
                                    return;
                                }
                                selected.set(Some(entry.path.clone()));
                                move_target.set(entry.path.clone());
                                match editor.asset_settings(&entry.path) {
                                    Ok(settings) => {
                                        fields.set(fields_from_settings(&settings));
                                        status.set(String::new());
                                    }
                                    Err(error) => {
                                        fields.set(Vec::new());
                                        status.set(error);
                                    }
                                }
                            }
                        },
                        div {
                            class: "editor-row-title",
                            if entry.is_directory { "▸ {entry.name}" } else { "{entry.name}" }
                        }
                        div { class: "editor-row-subtitle", "{row_subtitle(&entry)}" }
                    }
                }
            }
            if let Some(entry) = selected_entry
                .clone().filter(|entry| !entry.is_directory && entry.type_name.is_some()) {
                div {
                    class: "editor-asset-inspector",
                    div { class: "editor-row-title", "{entry.path}" }
                    for (index, field) in fields.read().clone().into_iter().enumerate() {
                        div {
                            key: "setting-{field.key}",
                            class: "editor-field",
                            label { "{field.key}" }
                            match field.kind {
                                SettingKind::Bool => rsx! {
                                    input {
                                        r#type: "checkbox",
                                        checked: field.text == "true",
                                        onchange: move |_| {
                                            let mut edited = fields.read().clone();
                                            let flipped = edited[index].text != "true";
                                            edited[index].text = flipped.to_string();
                                            fields.set(edited);
                                        },
                                    }
                                },
                                SettingKind::Json => rsx! {
                                    textarea {
                                        rows: "4",
                                        value: "{field.text}",
                                        oninput: move |event| {
                                            let mut edited = fields.read().clone();
                                            edited[index].text = event.value();
                                            fields.set(edited);
                                        },
                                    }
                                },
                                _ => rsx! {
                                    input {
                                        r#type: "text",
                                        value: "{field.text}",
                                        oninput: move |event| {
                                            let mut edited = fields.read().clone();
                                            edited[index].text = event.value();
                                            fields.set(edited);
                                        },
                                    }
                                },
                            }
                        }
                    }
                    div {
                        class: "editor-panel-toolbar",
                        button {
                            onclick: {
                                let editor = Arc::clone(&editor);
                                let path = entry.path.clone();
                                move |_| {
                                    let result = settings_from_fields(&fields.read())
                                        .and_then(|settings| editor.save_asset_settings(&path, settings));
                                    match result {
                                        Ok(saved) => {
                                            fields.set(fields_from_settings(&saved));
                                            status.set(format!("saved {path}; it reloads in the running scene"));
                                        }
                                        Err(error) => status.set(error),
                                    }
                                }
                            },
                            "Save settings"
                        }
                    }
                    div {
                        class: "editor-field",
                        label { "Move to" }
                        input {
                            r#type: "text",
                            value: "{move_target}",
                            oninput: move |event| move_target.set(event.value()),
                        }
                        button {
                            onclick: {
                                let editor = Arc::clone(&editor);
                                let path = entry.path.clone();
                                move |_| {
                                    let target = move_target.read().clone();
                                    match editor.move_asset(&path, &target) {
                                        Ok(()) => {
                                            selected.set(Some(target.clone()));
                                            status.set(format!("moved {path} to {target}, with its .meta"));
                                        }
                                        Err(error) => status.set(error),
                                    }
                                }
                            },
                            "Move"
                        }
                    }
                }
            }
            if !status.read().is_empty() {
                div { class: "editor-row-subtitle", "{status}" }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_texture_settings_object_becomes_fields_and_back() {
        let settings =
            serde_json::json!({"texture_type": "Color", "flip": true, "scale": 2, "bias": 0.5});
        let mut fields = fields_from_settings(&settings);
        let kinds: Vec<(&str, SettingKind)> = fields
            .iter()
            .map(|field| (field.key.as_str(), field.kind))
            .collect();
        assert_eq!(
            kinds,
            [
                ("bias", SettingKind::Number),
                ("flip", SettingKind::Bool),
                ("scale", SettingKind::Number),
                ("texture_type", SettingKind::Text),
            ]
        );

        fields[3].text = "Normal".to_owned();
        fields[1].text = "false".to_owned();
        assert_eq!(
            settings_from_fields(&fields).unwrap(),
            serde_json::json!({"texture_type": "Normal", "flip": false, "scale": 2, "bias": 0.5})
        );
    }

    #[test]
    fn nested_values_are_edited_as_json_and_bad_text_is_refused() {
        let settings = serde_json::json!({"textures": {"base": null}, "order": 3});
        let mut fields = fields_from_settings(&settings);
        assert_eq!(fields[1].kind, SettingKind::Json);
        assert_eq!(settings_from_fields(&fields).unwrap(), settings);

        fields[0].text = "three".to_owned();
        assert!(settings_from_fields(&fields).unwrap_err().contains("order"));
        fields[0].text = "3".to_owned();
        fields[1].text = "{ not json".to_owned();
        assert!(settings_from_fields(&fields)
            .unwrap_err()
            .contains("textures"));
    }

    #[test]
    fn an_empty_settings_object_has_no_fields() {
        assert!(fields_from_settings(&serde_json::json!({})).is_empty());
        assert_eq!(settings_from_fields(&[]).unwrap(), serde_json::json!({}));
    }

    fn entry(path: &str, is_directory: bool) -> AssetEntry {
        AssetEntry {
            path: path.to_owned(),
            name: path.rsplit('/').next().unwrap_or(path).to_owned(),
            depth: path.matches('/').count(),
            is_directory,
            type_name: None,
            standalone: false,
            loaded: false,
            guid: None,
        }
    }

    #[test]
    fn a_new_asset_defaults_to_the_selected_folder() {
        assert_eq!(folder_for_new_asset(None), "");
        assert_eq!(folder_for_new_asset(Some(&entry("passes", true))), "passes");
        assert_eq!(
            folder_for_new_asset(Some(&entry("textures/a.png", false))),
            "textures"
        );
        assert_eq!(
            folder_for_new_asset(Some(&entry("top.material", false))),
            ""
        );
    }

    #[test]
    fn the_preview_uses_the_hosts_naming_rule() {
        assert_eq!(
            preview_path("passes", "render_pass_xyz", "render_pass").unwrap(),
            "res/passes/render_pass_xyz.render_pass"
        );
        assert_eq!(
            preview_path("", "a.material", "material").unwrap(),
            "res/a.material"
        );
        assert!(preview_path("passes", "CON", "render_pass").is_err());
        assert!(preview_path("passes", "", "render_pass").is_err());
    }

    #[test]
    fn type_names_shorten_to_their_last_segment() {
        assert_eq!(
            short_type_name("pill_master_renderer::assets::Texture"),
            "Texture"
        );
        assert_eq!(short_type_name("Plain"), "Plain");
    }
}
