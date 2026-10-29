//! The Preferences tab's shared vocabulary: the preferences
//! file's editable keys, value parsing and the document helpers.
//!
//! # Responsibilities
//!
//! - [`PREFERENCE_FIELDS`]: the editable rows (file key, label, blank-file
//!   fallback) the Preferences tab draws, in row order.
//! - [`PreferencesView`]: the file's current values as display strings,
//!   plus the project key and its remembered gizmo entry - the snapshot
//!   tier the tab seeds its buffers from.
//! - [`parse_value`], [`set_key`], [`read_document`], [`write_document`]:
//!   the text-to-JSON rule and the dotted-key document access the
//!   `SetPreferences` command applies.
//!
//! # Design
//!
//! The preferences file is a plain JSON document the editor does not own
//! alone: every write is a read-modify-write, so keys other writers leave
//! behind - the executable's per-project gizmo entry - survive a save. Rows
//! carry the text the user typed; [`parse_value`] turns a row's text into
//! JSON - bare JSON first, so a number or a boolean round-trips, anything
//! else the string it looks like. A blank row writes nothing, so blank keeps
//! whatever the consumer's own default is.

// Standard library
use std::collections::BTreeMap;
use std::path::Path;

// External crates
use serde_json::{Map, Value};

// Current crate
use crate::commands::{GizmoProjectPreferences, KeymapPreset, PREFERENCES_FILE};

/// The tab's unsaved field buffers: one text per editable key.
pub type PreferencesDraft = BTreeMap<String, String>;

/// One editable preference: the document key and the row's label.
pub struct PreferenceField {
    /// The file's key; a dot nests into an object.
    pub key: &'static str,
    /// The label the row shows.
    pub label: &'static str,
    /// The text a file without the key shows - the consumer's own default.
    pub fallback: &'static str,
}

/// The editable rows, in the order the tab draws them.
pub const PREFERENCE_FIELDS: [PreferenceField; 12] = [
    PreferenceField {
        key: KEYMAP_PRESET_KEY,
        label: "keymap preset",
        fallback: "",
    },
    PreferenceField {
        key: crate::previews::RENDER_PREVIEWS_KEY,
        label: "render asset previews",
        fallback: "true",
    },
    PreferenceField {
        key: GIZMO_HIT_RADIUS_KEY,
        label: "gizmo hit radius",
        fallback: "0.07",
    },
    PreferenceField {
        key: GIZMO_ICON_SIZE_KEY,
        label: "gizmo icon size",
        // Two steps up from the fixed 10-point mark the badges carried
        // before the row existed, so the default reads clearly larger.
        fallback: "20",
    },
    PreferenceField {
        key: "code_editor_command",
        label: "code editor command",
        fallback: "",
    },
    PreferenceField {
        key: "autosave_interval_seconds",
        label: "autosave seconds",
        fallback: "",
    },
    PreferenceField {
        key: "snapping_steps.translation",
        label: "snap translate step",
        fallback: "0.5",
    },
    PreferenceField {
        key: "snapping_steps.rotation_degrees",
        label: "snap rotate degrees",
        fallback: "15",
    },
    PreferenceField {
        key: "snapping_steps.scale",
        label: "snap scale step",
        fallback: "0.1",
    },
    PreferenceField {
        key: "history_step_cap",
        label: "undo step cap",
        fallback: "",
    },
    PreferenceField {
        key: "history_memory_budget_mb",
        label: "undo memory MB",
        fallback: "",
    },
    PreferenceField {
        key: THEME_PREFERENCE_KEY,
        label: "theme file",
        fallback: "",
    },
];

/// The preference key the editor's user theme layer is named under: a
/// `.theme`/`.json` file, absolute or relative to the editor's state
/// directory.
pub const THEME_PREFERENCE_KEY: &str = "theme";

/// The preference key the keymap preset lives under.
pub const KEYMAP_PRESET_KEY: &str = "keymap_preset";

/// The keymap preset the file selects, or the default (Blender) when the
/// key is absent, blank or unknown - the preset the viewport navigation
/// table reads.
pub fn keymap_preset(document: &Map<String, Value>) -> KeymapPreset {
    let Some(Value::String(name)) = lookup(document, KEYMAP_PRESET_KEY) else {
        return KeymapPreset::default();
    };
    match name.trim().to_ascii_lowercase().as_str() {
        "blender" => KeymapPreset::Blender,
        "unity" => KeymapPreset::Unity,
        "godot" => KeymapPreset::Godot,
        "unreal" => KeymapPreset::Unreal,
        _ => KeymapPreset::default(),
    }
}

/// The gizmo handle's hit radius the file asks for, as a multiplier on the
/// handle length.
///
/// The hover test and the press both read this one number, so a handle that
/// highlights under the pointer is a handle a press grabs. A missing, blank
/// or unparsable value keeps the default, and a value outside
/// [`GIZMO_HIT_RADIUS_RANGE`] is clamped rather than trusted: a zero radius
/// would make the gizmo ungrabbable.
pub fn gizmo_hit_radius(document: &Map<String, Value>) -> f32 {
    let value = match lookup(document, GIZMO_HIT_RADIUS_KEY) {
        Some(Value::Number(number)) => number.as_f64().map(|number| number as f32),
        Some(Value::String(text)) => text.trim().parse::<f32>().ok(),
        _ => None,
    };
    value
        .filter(|radius| radius.is_finite())
        .unwrap_or(GIZMO_HIT_RADIUS_DEFAULT)
        .clamp(GIZMO_HIT_RADIUS_RANGE.0, GIZMO_HIT_RADIUS_RANGE.1)
}

/// The preference key the gizmo's hit radius lives under.
pub const GIZMO_HIT_RADIUS_KEY: &str = "editor.gizmo_hit_radius";

/// The hit radius used when the file says nothing: about the width the
/// overlay draws a handle's line with.
pub const GIZMO_HIT_RADIUS_DEFAULT: f32 = 0.07;

/// The range a file's hit radius is clamped into, as multipliers on the
/// handle length: tight enough to still mean a handle, wide enough to grab
/// from.
pub const GIZMO_HIT_RADIUS_RANGE: (f32, f32) = (0.01, 0.35);

/// The preference key the scene badges' icon size lives under.
pub const GIZMO_ICON_SIZE_KEY: &str = "editor.gizmo_icon_size";

/// The icon size used when the file says nothing, in points: twice the fixed
/// 10-point mark the badges drew before the preference existed.
pub const GIZMO_ICON_SIZE_DEFAULT: f32 = 20.0;

/// The range a file's icon size is clamped into, in points: small enough to
/// stay a mark over the scene, large enough to keep it clickable.
pub const GIZMO_ICON_SIZE_RANGE: (f32, f32) = (8.0, 64.0);

/// The scene badges' icon size the file asks for, in points.
///
/// The mark and the rectangle that picks it both scale with this one number.
/// A missing, blank or unparsable value keeps the default, and a value outside
/// [`GIZMO_ICON_SIZE_RANGE`] is clamped rather than trusted: a zero or
/// negative size would draw nothing to click on.
pub fn gizmo_icon_size(document: &Map<String, Value>) -> f32 {
    let value = match lookup(document, GIZMO_ICON_SIZE_KEY) {
        Some(Value::Number(number)) => number.as_f64().map(|number| number as f32),
        Some(Value::String(text)) => text.trim().parse::<f32>().ok(),
        _ => None,
    };
    value
        .filter(|size| size.is_finite())
        .unwrap_or(GIZMO_ICON_SIZE_DEFAULT)
        .clamp(GIZMO_ICON_SIZE_RANGE.0, GIZMO_ICON_SIZE_RANGE.1)
}

/// The file's current values as the tab shows them.
#[derive(Clone, Debug, PartialEq)]
pub struct PreferencesView {
    /// One display string per editable field, keyed like the fields.
    pub values: BTreeMap<String, String>,
    /// The scene badges' icon size, in points: the shell's drawing needs the
    /// number, not the row's text, so it is parsed here from the same
    /// document the rows show.
    pub gizmo_icon_size: f32,
    /// The project key the per-project entry is remembered under.
    pub project: Option<String>,
    /// The project's remembered gizmo entry, as the file holds it (the
    /// defaults when the file has none).
    pub project_gizmo: GizmoProjectPreferences,
    /// Whether the file carries the project's entry at all.
    pub project_remembered: bool,
}

impl Default for PreferencesView {
    fn default() -> Self {
        Self {
            values: BTreeMap::new(),
            gizmo_icon_size: GIZMO_ICON_SIZE_DEFAULT,
            project: None,
            project_gizmo: GizmoProjectPreferences::default(),
            project_remembered: false,
        }
    }
}

impl PreferencesView {
    /// Build the view from one parsed document.
    pub fn from_document(document: &Map<String, Value>, project: Option<&str>) -> Self {
        let values = PREFERENCE_FIELDS
            .iter()
            .map(|field| {
                let text = lookup(document, field.key)
                    .map(display_value)
                    .unwrap_or_else(|| field.fallback.to_string());
                (field.key.to_string(), text)
            })
            .collect();
        let project_entry = project.and_then(|name| {
            let projects = lookup(document, "gizmo.projects")?.as_object()?;
            projects.get(name)
        });
        let project_gizmo = project_entry
            .and_then(|entry| serde_json::from_value(entry.clone()).ok())
            .unwrap_or_default();
        Self {
            values,
            gizmo_icon_size: gizmo_icon_size(document),
            project: project.map(str::to_string),
            project_gizmo,
            project_remembered: project_entry.is_some(),
        }
    }

    /// A copy of the rows the tab's buffers start from.
    pub fn seed_buffers(&self) -> BTreeMap<String, String> {
        self.values.clone()
    }

    /// One row's display text.
    pub fn text(&self, key: &str) -> &str {
        self.values.get(key).map(String::as_str).unwrap_or("")
    }
}

/// Follow a dotted key through the document; `None` when any hop is absent
/// or not an object.
pub fn lookup<'a>(document: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    let mut segments = key.split('.').peekable();
    let mut current = document;
    while let Some(segment) = segments.next() {
        let value = current.get(segment)?;
        if segments.peek().is_none() {
            return Some(value);
        }
        current = value.as_object()?;
    }
    None
}

/// One JSON value as the tab's text: strings verbatim, the rest compact.
fn display_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Parse one row's text into JSON: bare JSON first (numbers, booleans,
/// arrays), anything else the string it looks like.
pub fn parse_value(text: &str) -> Value {
    let trimmed = text.trim();
    serde_json::from_str::<Value>(trimmed)
        .or_else(|_| serde_json::from_str::<Value>(&format!("\"{trimmed}\"")))
        .unwrap_or_else(|_| Value::String(trimmed.to_string()))
}

/// Set one dotted key, creating intermediate objects on the way.
///
/// # Errors
///
/// A message when a segment on the way is present but not an object, or the
/// key is empty.
pub fn set_key(document: &mut Map<String, Value>, key: &str, value: Value) -> Result<(), String> {
    if key.is_empty() {
        return Err(format!("`{key}` is empty"));
    }
    let segments: Vec<&str> = key.split('.').collect();
    let Some((last, parents)) = segments.split_last() else {
        return Err(format!("`{key}` is empty"));
    };
    let mut current = document;
    for segment in parents {
        let entry = current
            .entry(segment.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        current = entry
            .as_object_mut()
            .ok_or_else(|| format!("`{segment}` in `{key}` is not an object"))?;
    }
    current.insert(last.to_string(), value);
    Ok(())
}

/// Read the preferences document a write will modify.
///
/// A missing file is an empty document - the first save creates it - but a file
/// that exists and cannot be read or parsed is an error. Folding it into an
/// empty document would let the next save write a file holding only the keys it
/// set, erasing every unrelated preference.
///
/// # Errors
///
/// A message naming the file when it cannot be read, is not JSON, or is not a
/// JSON object.
pub fn read_document(directory: &Path) -> Result<Map<String, Value>, String> {
    let path = directory.join(PREFERENCES_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(error) => return Err(format!("{PREFERENCES_FILE}: {error}")),
    };
    parse_document(&text)
}

/// Parse a preferences file's text into its document.
///
/// # Errors
///
/// A message naming the file when the text is not JSON or is not a JSON object.
pub fn parse_document(text: &str) -> Result<Map<String, Value>, String> {
    let value: Value = serde_json::from_str(text)
        .map_err(|error| format!("{PREFERENCES_FILE} is not valid JSON: {error}"))?;
    match value {
        Value::Object(document) => Ok(document),
        _ => Err(format!("{PREFERENCES_FILE} is not a JSON object")),
    }
}

/// Write the document through a temporary file and a rename, creating the
/// directory when the profile never saved a layout.
///
/// # Errors
///
/// A message when the document cannot be serialized or the file cannot be
/// replaced.
pub fn write_document(directory: &Path, document: &Map<String, Value>) -> Result<(), String> {
    let text = serde_json::to_string_pretty(&Value::Object(document.clone()))
        .map_err(|error| error.to_string())?;
    std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    let path = directory.join(PREFERENCES_FILE);
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, text).map_err(|error| error.to_string())?;
    std::fs::rename(&temporary, &path).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory of this test's own under the system temp
    /// directory, named so parallel runs do not collide.
    fn scratch_directory(label: &str) -> std::path::PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "ecs-preferences-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    /// Every editable row's key is one the document helpers can write and read
    /// back: the table, the dotted-key path and the reader agree by
    /// construction, so a duplicated or malformed key fails here rather than as
    /// a row that saves a value nothing reads.
    #[test]
    fn every_preference_field_round_trips_through_its_key() {
        for field in &PREFERENCE_FIELDS {
            let mut document = Map::new();
            set_key(&mut document, field.key, serde_json::json!("probe"))
                .unwrap_or_else(|error| panic!("{} cannot be written: {error}", field.key));
            assert_eq!(
                lookup(&document, field.key),
                Some(&serde_json::json!("probe")),
                "{} did not read back what the table wrote",
                field.key
            );
            let view = PreferencesView::from_document(&document, None);
            assert_eq!(view.text(field.key), "probe", "{} has no row", field.key);
        }
    }

    #[test]
    fn text_parses_as_bare_json_first_then_a_string() {
        assert_eq!(parse_value("0.3"), serde_json::json!(0.3));
        assert_eq!(parse_value("42"), serde_json::json!(42));
        assert_eq!(parse_value("true"), serde_json::json!(true));
        assert_eq!(parse_value("code ."), serde_json::json!("code ."));
        assert_eq!(parse_value(""), serde_json::json!(""));
    }

    #[test]
    fn the_gizmo_hit_radius_reads_numbers_strings_and_clamps() {
        let mut document = Map::new();
        // Absent: the default.
        assert_eq!(gizmo_hit_radius(&document), GIZMO_HIT_RADIUS_DEFAULT);
        // A number and a numeric string both read.
        set_key(&mut document, GIZMO_HIT_RADIUS_KEY, serde_json::json!(0.12)).unwrap();
        assert_eq!(gizmo_hit_radius(&document), 0.12);
        set_key(
            &mut document,
            GIZMO_HIT_RADIUS_KEY,
            serde_json::json!("0.2"),
        )
        .unwrap();
        assert_eq!(gizmo_hit_radius(&document), 0.2);
        // A value outside the range is clamped, and junk keeps the default.
        set_key(&mut document, GIZMO_HIT_RADIUS_KEY, serde_json::json!(40.0)).unwrap();
        assert_eq!(gizmo_hit_radius(&document), GIZMO_HIT_RADIUS_RANGE.1);
        set_key(
            &mut document,
            GIZMO_HIT_RADIUS_KEY,
            serde_json::json!("wide"),
        )
        .unwrap();
        assert_eq!(gizmo_hit_radius(&document), GIZMO_HIT_RADIUS_DEFAULT);
    }

    #[test]
    fn the_gizmo_icon_size_defaults_doubles_and_clamps() {
        let mut document = Map::new();
        // Absent: the default, twice the 10-point mark the badges drew before
        // the row existed.
        assert_eq!(gizmo_icon_size(&document), GIZMO_ICON_SIZE_DEFAULT);
        // A number and a numeric string both read.
        set_key(&mut document, GIZMO_ICON_SIZE_KEY, serde_json::json!(28)).unwrap();
        assert_eq!(gizmo_icon_size(&document), 28.0);
        set_key(&mut document, GIZMO_ICON_SIZE_KEY, serde_json::json!("36")).unwrap();
        assert_eq!(gizmo_icon_size(&document), 36.0);
        // A value outside the range is clamped, and junk keeps the default.
        set_key(&mut document, GIZMO_ICON_SIZE_KEY, serde_json::json!(0)).unwrap();
        assert_eq!(gizmo_icon_size(&document), GIZMO_ICON_SIZE_RANGE.0);
        set_key(&mut document, GIZMO_ICON_SIZE_KEY, serde_json::json!(900)).unwrap();
        assert_eq!(gizmo_icon_size(&document), GIZMO_ICON_SIZE_RANGE.1);
        set_key(&mut document, GIZMO_ICON_SIZE_KEY, serde_json::json!("big")).unwrap();
        assert_eq!(gizmo_icon_size(&document), GIZMO_ICON_SIZE_DEFAULT);
        // The view carries the parsed size beside the row's text.
        set_key(&mut document, GIZMO_ICON_SIZE_KEY, serde_json::json!(24)).unwrap();
        let view = PreferencesView::from_document(&document, None);
        assert_eq!(view.text(GIZMO_ICON_SIZE_KEY), "24");
        assert_eq!(view.gizmo_icon_size, 24.0);
        assert_eq!(
            PreferencesView::default().gizmo_icon_size,
            GIZMO_ICON_SIZE_DEFAULT
        );
    }

    #[test]
    fn set_key_creates_nested_objects_and_refuses_scalar_hops() {
        let mut document = Map::new();
        set_key(&mut document, "history_step_cap", serde_json::json!(42)).unwrap();
        set_key(
            &mut document,
            "snapping_steps.translation",
            serde_json::json!(0.25),
        )
        .unwrap();
        assert_eq!(document["history_step_cap"], serde_json::json!(42));
        assert_eq!(
            document["snapping_steps"]["translation"],
            serde_json::json!(0.25)
        );
        assert_eq!(
            set_key(
                &mut document,
                "history_step_cap.deeper",
                serde_json::json!(1)
            ),
            Err("`history_step_cap` in `history_step_cap.deeper` is not an object".to_string())
        );
        assert_eq!(
            set_key(&mut document, "", serde_json::json!(1)),
            Err("`` is empty".to_string())
        );
    }

    #[test]
    fn the_view_reads_values_fallbacks_and_the_project_entry() {
        let mut document = Map::new();
        set_key(&mut document, "keymap_preset", serde_json::json!("preset")).unwrap();
        set_key(
            &mut document,
            "snapping_steps.translation",
            serde_json::json!(0.25),
        )
        .unwrap();
        set_key(
            &mut document,
            "gizmo.projects.italian_brainrot.mode",
            serde_json::json!("rotate"),
        )
        .unwrap();
        let view = PreferencesView::from_document(&document, Some("italian_brainrot"));
        assert_eq!(view.text("keymap_preset"), "preset");
        assert_eq!(view.text("snapping_steps.translation"), "0.25");
        // Absent keys show the consumer's own fallbacks.
        assert_eq!(view.text("snapping_steps.rotation_degrees"), "15");
        assert_eq!(view.text("history_step_cap"), "");
        assert_eq!(view.text("code_editor_command"), "");
        assert!(view.project_remembered);
        assert_eq!(
            view.project_gizmo.mode,
            Some(crate::gizmo::GizmoMode::Rotate)
        );
        // A project the file never saw reads as unremembered defaults.
        let other = PreferencesView::from_document(&document, Some("project_rs"));
        assert!(!other.project_remembered);
        assert_eq!(other.project_gizmo.mode, None);
        assert_eq!(other.project.as_deref(), Some("project_rs"));
    }

    #[test]
    fn the_document_round_trips_through_the_file() {
        let directory = scratch_directory("round-trip");
        let mut document = read_document(&directory).unwrap();
        assert!(document.is_empty());
        set_key(&mut document, "theme", serde_json::json!("dark")).unwrap();
        set_key(
            &mut document,
            "snapping_steps.scale",
            serde_json::json!(0.2),
        )
        .unwrap();
        write_document(&directory, &document).unwrap();

        let read_back = read_document(&directory).unwrap();
        assert_eq!(read_back["theme"], serde_json::json!("dark"));
        assert_eq!(read_back["snapping_steps"]["scale"], serde_json::json!(0.2));
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_corrupt_document_is_refused_rather_than_read_as_empty() {
        // Folding "not JSON" into an empty document would let the next save
        // erase every key the file held, including the ones this write never
        // touched.
        let directory = scratch_directory("corrupt");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(PREFERENCES_FILE);
        let corrupt = "{\"theme\": \"dark\",}";
        std::fs::write(&path, corrupt).unwrap();

        let error = read_document(&directory).expect_err("a trailing comma is not a document");
        assert!(
            error.contains("not valid JSON"),
            "unexpected message: {error}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            corrupt,
            "the file is left exactly as it was"
        );

        // A missing file is still an empty document: the first save creates it.
        std::fs::remove_file(&path).unwrap();
        assert!(read_document(&directory)
            .expect("missing reads empty")
            .is_empty());
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_document_that_is_not_an_object_is_refused() {
        // A file holding `[1, 2]` parses as JSON but is not a document, so a
        // save that modified it would have nowhere to put a key.
        let error = parse_document("[1, 2]").expect_err("a list is not a document");
        assert!(
            error.contains("not a JSON object"),
            "unexpected message: {error}"
        );
        assert!(parse_document("{\"theme\": \"dark\"}")
            .expect("an object is a document")
            .contains_key("theme"));
    }
}
