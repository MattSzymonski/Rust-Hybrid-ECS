//! The project's assets as a tool sees them: the `res` tree, each file's
//! asset type, load state and guid, and the operations an editor performs on
//! them.
//!
//! # Responsibilities
//!
//! - List the project's `res` directory as a tree ([`list_assets`]): folders
//!   and files, `.meta` sidecars hidden, each file with its asset type, whether
//!   it is loaded, and its guid.
//! - Read an asset's import settings - or a standalone asset's own document -
//!   as JSON ([`asset_settings`]), and save edited ones back to disk
//!   ([`save_asset_settings`]).
//! - Move or rename an asset together with its `.meta` ([`move_asset`]).
//! - Create a new standalone asset - a material, a render pass - from its
//!   type's default document ([`create_standalone_asset`]), with every check
//!   on the name and folder done here rather than in a UI.
//! - Refuse every path outside `res`.
//!
//! # Design
//!
//! Everything goes through the world's
//! [`ImportRegistry`](pill_engine::ImportRegistry), so no asset type is named
//! here: a type a module registers later shows up and edits with no change to
//! this file or to the editor.
//!
//! Nothing here reimports anything. Saving rewrites the file atomically and a
//! move renames files; the dev host's asset watcher sees the change and
//! reimports (or follows the move) at the next frame boundary, exactly as it
//! does for an edit made in any other program. One path for every edit keeps
//! the editor from being a special case of the live-reload rules.

// Standard library
use std::path::{Component, Path, PathBuf};

// External crates
use pill_engine::asset_metadata::METADATA_FORMAT_VERSION;
use pill_engine::{AssetGuid, AssetManager, ImportRegistry, MetadataPolicy, World};

/// The suffix of a metadata sidecar.
const METADATA_SUFFIX: &str = ".meta";

/// One row of the `res` tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetEntry {
    /// The path relative to `res`, `/`-separated; a folder's has no trailing
    /// slash.
    pub path: String,
    /// The last component of [`Self::path`].
    pub name: String,
    /// How many folders deep the entry is (`0` directly in `res`).
    pub depth: usize,
    /// Whether this is a folder.
    pub is_directory: bool,
    /// The registered asset type that imports the file, if any.
    pub type_name: Option<String>,
    /// Whether the file is a standalone asset (it holds its own guid).
    pub standalone: bool,
    /// Whether an asset is loaded under this path.
    pub loaded: bool,
    /// The guid the file's `.meta` - or its own header - holds, if any.
    pub guid: Option<String>,
}

/// Every folder and file under `asset_directory`, depth first, folders before
/// files and each level sorted by name; `.meta` files and dot-files left out.
pub fn list_assets(world: &World, asset_directory: &Path) -> Vec<AssetEntry> {
    let registry = world.get_resource::<ImportRegistry>();
    let assets = world.get_resource::<AssetManager>();
    let mut entries = Vec::new();
    visit(
        asset_directory,
        Path::new(""),
        0,
        &mut |relative, is_directory, depth| {
            let path = slash_path(relative);
            let name = relative
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let mut entry = AssetEntry {
                path,
                name,
                depth,
                is_directory,
                type_name: None,
                standalone: false,
                loaded: false,
                guid: None,
            };
            if !is_directory {
                let extension = relative
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .unwrap_or_default();
                if let Some(registry) = registry {
                    entry.type_name = registry.type_for_extension(extension).map(str::to_owned);
                    entry.standalone = registry.is_standalone_extension(extension);
                    entry.loaded =
                        assets.is_some_and(|assets| registry.is_loaded(assets, relative));
                }
                let header = if entry.standalone {
                    asset_directory.join(relative)
                } else {
                    asset_directory.join(format!("{}{METADATA_SUFFIX}", relative.display()))
                };
                entry.guid = read_guid(&header);
            }
            entries.push(entry);
        },
    );
    entries
}

/// Walk `root/relative` recursively, calling `visit(relative, is_directory,
/// depth)` for each folder and file in the order [`list_assets`] promises.
fn visit(
    root: &Path,
    relative: &Path,
    depth: usize,
    visit_entry: &mut dyn FnMut(&Path, bool, usize),
) {
    let Ok(read) = std::fs::read_dir(root.join(relative)) else {
        return;
    };
    let mut folders = Vec::new();
    let mut files = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name();
        let text = name.to_string_lossy();
        if text.starts_with('.') || text.ends_with(METADATA_SUFFIX) {
            continue;
        }
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => folders.push(relative.join(&name)),
            Ok(kind) if kind.is_file() => files.push(relative.join(&name)),
            _ => {}
        }
    }
    folders.sort();
    files.sort();
    for folder in folders {
        visit_entry(&folder, true, depth);
        visit(root, &folder, depth + 1, visit_entry);
    }
    for file in files {
        visit_entry(&file, false, depth);
    }
}

/// The `guid` field of the JSON asset header in `path`, if it has one.
fn read_guid(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let document: serde_json::Value = serde_json::from_str(&text).ok()?;
    document.get("guid")?.as_str().map(str::to_owned)
}

/// `path` as a `/`-separated string.
fn slash_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// `path` checked to stay inside `res`: relative, without `..`, and naming
/// something. Returns it as a `PathBuf`.
///
/// # Errors
///
/// A message naming the path when it is absolute, climbs out, or is empty.
pub fn checked_relative(path: &str) -> Result<PathBuf, String> {
    let candidate = PathBuf::from(path.replace('\\', "/"));
    let mut normal = PathBuf::new();
    for component in candidate.components() {
        match component {
            Component::Normal(part) => normal.push(part),
            Component::CurDir => {}
            _ => return Err(format!("`{path}` is not a path inside `res`")),
        }
    }
    if normal.as_os_str().is_empty() {
        return Err("the path is empty".to_owned());
    }
    Ok(normal)
}

/// The import settings of the asset at `path` - or, for a standalone asset,
/// its own document - as JSON: what the file holds, or the type's defaults.
///
/// # Errors
///
/// A message when no registered type imports the path or its file cannot be
/// read.
pub fn asset_settings(world: &World, path: &str) -> Result<serde_json::Value, String> {
    let relative = checked_relative(path)?;
    let registry = world
        .get_resource::<ImportRegistry>()
        .ok_or("no asset types are registered")?;
    registry
        .settings_json(&relative)
        .map_err(|error| error.to_string())
}

/// Save `settings` as the import settings of the asset at `path` - or, for a
/// standalone asset, as its document - and return the normalized value that
/// was written.
///
/// The value is checked against the type first (missing fields defaulted,
/// unknown ones dropped), so a malformed edit is refused before anything is
/// written. A sourced asset without a `.meta` yet is imported first, which
/// writes one. The file is replaced atomically; the asset watcher then
/// reimports the asset in place.
///
/// # Errors
///
/// A message when the path leaves `res`, no type imports it, the settings do
/// not fit the type, or the file cannot be read or written.
pub fn save_asset_settings(
    world: &mut World,
    asset_directory: &Path,
    path: &str,
    settings: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let relative = checked_relative(path)?;
    let registry = world
        .get_resource::<ImportRegistry>()
        .cloned()
        .ok_or("no asset types are registered")?;
    let extension = relative
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_owned();
    let type_name = registry
        .type_for_extension(&extension)
        .ok_or_else(|| format!("no asset type imports `.{extension}` files"))?
        .to_owned();
    let normalized = registry
        .normalize_settings_json(&type_name, settings)
        .map_err(|error| error.to_string())?;

    let standalone = registry.is_standalone_extension(&extension);
    let (file, body_field) = if standalone {
        (asset_directory.join(&relative), "asset")
    } else {
        (
            asset_directory.join(format!("{}{METADATA_SUFFIX}", relative.display())),
            "settings",
        )
    };
    if !standalone && !file.is_file() {
        // The import writes the `.meta` with the type's defaults and a guid,
        // which the settings below then replace.
        let assets = world
            .get_resource_mut::<AssetManager>()
            .ok_or("the engine has no AssetManager")?;
        registry
            .import(assets, &relative, MetadataPolicy::CreateIfMissing)
            .map_err(|error| error.to_string())?;
    }

    let text = std::fs::read_to_string(&file)
        .map_err(|error| format!("cannot read `{}`: {error}", file.display()))?;
    let mut document: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| format!("`{}` is not valid JSON: {error}", file.display()))?;
    let object = document
        .as_object_mut()
        .ok_or_else(|| format!("`{}` is not a JSON object", file.display()))?;
    object.insert(body_field.to_owned(), normalized.clone());
    let mut bytes = serde_json::to_vec_pretty(&document).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    replace_atomically(&file, &bytes)?;
    Ok(normalized)
}

/// Write `bytes` to a temporary sibling of `file`, then rename it over `file`,
/// so a reader never sees a half-written file.
fn replace_atomically(file: &Path, bytes: &[u8]) -> Result<(), String> {
    let file_name = file
        .file_name()
        .ok_or_else(|| format!("`{}` has no file name", file.display()))?;
    let temporary = file.with_file_name(format!(
        ".{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id()
    ));
    std::fs::write(&temporary, bytes)
        .map_err(|error| format!("cannot write `{}`: {error}", temporary.display()))?;
    std::fs::rename(&temporary, file).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        format!("cannot replace `{}`: {error}", file.display())
    })
}

/// Move the asset at `from` to `to` (both relative to `res`) together with
/// its `.meta`, through [`pill_assets::move_asset`]. The asset watcher then
/// follows the move: the loaded asset keeps its handle and guid.
///
/// # Errors
///
/// A message when either path leaves `res`, or the move is refused (missing
/// source, existing target).
pub fn move_asset(asset_directory: &Path, from: &str, to: &str) -> Result<(), String> {
    let from = checked_relative(from)?;
    let to = checked_relative(to)?;
    pill_assets::move_asset(&asset_directory.join(from), &asset_directory.join(to))
        .map_err(|error| error.to_string())
}

// =============================================================================
// Creating standalone assets
// =============================================================================

/// A standalone asset type a new file can be created for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StandaloneType {
    /// The registered type name, written to the file's `asset_type`.
    pub type_name: String,
    /// The last `::` segment of the type name, for a dropdown.
    pub display_name: String,
    /// The file extension, without the dot (`render_pass`).
    pub extension: String,
}

/// The live standalone types, sorted by display name: what a "create asset"
/// dropdown offers. Sourced types (textures, meshes, sounds) are not listed;
/// they come from adding a source file to `res`.
pub fn standalone_types(world: &World) -> Vec<StandaloneType> {
    let Some(registry) = world.get_resource::<ImportRegistry>() else {
        return Vec::new();
    };
    let mut types: Vec<StandaloneType> = registry
        .standalone_types()
        .into_iter()
        .map(|(type_name, extension)| StandaloneType {
            display_name: type_name
                .rsplit("::")
                .next()
                .unwrap_or(&type_name)
                .to_owned(),
            type_name,
            extension,
        })
        .collect();
    types.sort_by(|left, right| left.display_name.cmp(&right.display_name));
    types
}

/// Names Windows refuses as a file's base name, whatever the extension.
const RESERVED_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Characters Windows refuses in a file name.
const INVALID_CHARACTERS: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// The file name a new asset called `name` gets with `extension`: `name.extension`,
/// unless `name` already ends with that extension.
///
/// # Errors
///
/// A message when `name` is empty, contains a path separator or a character
/// Windows refuses, is `.` or `..`, ends with a dot or a space, or is a reserved
/// Windows name (`CON`, `NUL`, `COM1`, ...).
pub fn standalone_file_name(name: &str, extension: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("the name is empty".to_owned());
    }
    if let Some(character) = name
        .chars()
        .find(|character| INVALID_CHARACTERS.contains(character) || character.is_control())
    {
        return Err(format!("the name cannot contain `{character}`"));
    }
    if name == "." || name == ".." || name.contains("..") {
        return Err("the name cannot contain `..`".to_owned());
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err("the name cannot end with a dot or a space".to_owned());
    }
    let suffix = format!(".{extension}");
    let file_name = if name
        .to_ascii_lowercase()
        .ends_with(&suffix.to_ascii_lowercase())
    {
        name.to_owned()
    } else {
        format!("{name}{suffix}")
    };
    let base = file_name.split('.').next().unwrap_or_default();
    if RESERVED_NAMES
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(base))
    {
        return Err(format!("`{base}` is a name Windows reserves"));
    }
    Ok(file_name)
}

/// Create a new standalone asset of `type_name` called `name` in `folder`
/// (relative to `res`; empty for `res` itself), and load it. Returns its path
/// relative to `res`.
///
/// The file holds the standard header - format version, type name, a new
/// random guid - and the type's default document, which is a valid asset on
/// its own. It is written with [`pill_engine::asset_store::write_new`], so an
/// existing file is never replaced, then imported through the registry, so
/// the caller gets a loaded asset at once. When the asset watcher sees the new
/// file afterwards, the import finds it already loaded.
///
/// # Errors
///
/// A message when the type is not a live standalone type, the folder leaves
/// `res` or does not exist, the name is refused (see [`standalone_file_name`]),
/// the file already exists, or it cannot be written or loaded.
pub fn create_standalone_asset(
    world: &mut World,
    asset_directory: &Path,
    type_name: &str,
    folder: &str,
    name: &str,
) -> Result<String, String> {
    let registry = world
        .get_resource::<ImportRegistry>()
        .cloned()
        .ok_or("no asset types are registered")?;
    let (_, extension) = registry
        .standalone_types()
        .into_iter()
        .find(|(registered, _)| registered == type_name)
        .ok_or_else(|| format!("`{type_name}` is not a standalone asset type"))?;

    let folder = folder.trim().trim_matches('/');
    let relative_folder = if folder.is_empty() {
        PathBuf::new()
    } else {
        checked_relative(folder)?
    };
    let directory = asset_directory.join(&relative_folder);
    if !directory.is_dir() {
        return Err(format!(
            "the folder `res/{}` does not exist",
            slash_path(&relative_folder)
        ));
    }
    let file_name = standalone_file_name(name, &extension)?;
    let relative = relative_folder.join(&file_name);
    let destination = asset_directory.join(&relative);
    if destination.exists() {
        return Err(format!("`res/{}` already exists", slash_path(&relative)));
    }

    let document = registry
        .default_document(type_name)
        .map_err(|error| error.to_string())?;
    let guid =
        AssetGuid::random().map_err(|error| format!("no random source for a guid: {error}"))?;
    let file = serde_json::json!({
        "format_version": METADATA_FORMAT_VERSION,
        "asset_type": type_name,
        "guid": guid,
        "asset": document,
    });
    let mut bytes = serde_json::to_vec_pretty(&file).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    pill_engine::asset_store::write_new(&destination, &bytes).map_err(|error| error.to_string())?;

    let assets = world
        .get_resource_mut::<AssetManager>()
        .ok_or("the engine has no AssetManager")?;
    registry
        .import(assets, &relative, MetadataPolicy::ReadIfPresent)
        .map_err(|error| {
            format!(
                "`res/{}` was written but did not load: {error}",
                slash_path(&relative)
            )
        })?;
    Ok(slash_path(&relative))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pill_engine::{ImportedAsset, StandaloneAsset};

    /// Serializes the tests that mount a scratch `res`: the mount is one
    /// process-wide value, and tests run on parallel threads.
    fn mount_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A standalone test asset: a note with a default text.
    #[derive(Debug)]
    struct Note {
        text: String,
    }
    impl pill_engine::Asset for Note {}
    trait_type_map::impl_trait_accessible!(dyn pill_engine::Asset; Note);

    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    #[serde(default)]
    struct NoteDocument {
        text: String,
    }

    impl Default for NoteDocument {
        fn default() -> Self {
            Self {
                text: "new note".to_owned(),
            }
        }
    }

    impl pill_engine::StandaloneAsset for Note {
        type Document = NoteDocument;
        const FILE_EXTENSION: &'static str = "note";

        fn metadata_type_name() -> &'static str {
            "pill_host::asset_browser::tests::Note"
        }

        fn from_document(
            _name: &str,
            document: NoteDocument,
            _assets: &AssetManager,
        ) -> pill_engine::AssetLoadResult<Self> {
            Ok(Self {
                text: document.text,
            })
        }
    }

    /// A world with the standalone `Note` and the sourced `Swatch` registered,
    /// over a scratch `res` holding a `notes` folder.
    fn create_fixture(label: &str) -> (World, PathBuf) {
        // Callers hold `mount_lock` for the whole test.
        let root =
            std::env::temp_dir().join(format!("pill-asset-create-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("notes")).unwrap();
        pill_engine::asset_store::mount_directory(&root);
        let mut world = World::new();
        world.insert_resource(AssetManager::new());
        world.register_standalone_asset::<Note>();
        world.register_imported_asset::<Swatch>();
        (world, root)
    }

    #[test]
    fn only_standalone_types_are_offered() {
        let _mounted = mount_lock();
        let (world, root) = create_fixture("types");
        let types = standalone_types(&world);
        assert_eq!(
            types,
            [StandaloneType {
                type_name: Note::metadata_type_name().to_owned(),
                display_name: "Note".to_owned(),
                extension: "note".to_owned(),
            }]
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_created_asset_has_a_header_a_fresh_guid_and_is_loaded() {
        let _mounted = mount_lock();
        let (mut world, root) = create_fixture("create");

        let first = create_standalone_asset(
            &mut world,
            &root,
            Note::metadata_type_name(),
            "notes",
            "todo",
        )
        .unwrap();
        let second = create_standalone_asset(
            &mut world,
            &root,
            Note::metadata_type_name(),
            "",
            "idea.note",
        )
        .unwrap();

        assert_eq!(first, "notes/todo.note");
        assert_eq!(
            second, "idea.note",
            "an extension the user typed is not doubled"
        );
        let file: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(root.join("notes/todo.note")).unwrap())
                .unwrap();
        assert_eq!(file["format_version"], METADATA_FORMAT_VERSION);
        assert_eq!(file["asset_type"], Note::metadata_type_name());
        assert_eq!(file["asset"], serde_json::json!({"text": "new note"}));
        let guid = file["guid"].as_str().unwrap();
        assert!(AssetGuid::parse(guid).is_some());
        let other: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(root.join("idea.note")).unwrap())
                .unwrap();
        assert_ne!(other["guid"], file["guid"], "every file gets its own guid");
        assert!(
            !root.join("notes/todo.note.meta").exists(),
            "a standalone file has no .meta"
        );

        let assets = world.get_resource::<AssetManager>().unwrap();
        let handle = assets
            .handle_by_name::<Note>("notes/todo.note")
            .expect("loaded at once");
        assert_eq!(assets.get(handle).unwrap().text, "new note");
        assert_eq!(assets.guid_of(handle).unwrap().to_string(), guid);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn creation_refuses_bad_input_and_never_overwrites() {
        let _mounted = mount_lock();
        let (mut world, root) = create_fixture("refuse");
        let note = Note::metadata_type_name();
        create_standalone_asset(&mut world, &root, note, "notes", "taken").unwrap();
        let original = std::fs::read(root.join("notes/taken.note")).unwrap();

        let refusals = [
            (note, "notes", "taken", "already exists"),
            (note, "../outside", "a", "not a path inside"),
            (note, "missing", "a", "does not exist"),
            (note, "notes", "", "empty"),
            (note, "notes", "a/b", "cannot contain"),
            (note, "notes", "a\\b", "cannot contain"),
            (note, "notes", "..", "cannot contain"),
            (note, "notes", "what?", "cannot contain"),
            (note, "notes", "CON", "reserves"),
            (note, "notes", "nul.note", "reserves"),
            (
                Swatch::metadata_type_name(),
                "notes",
                "a",
                "not a standalone",
            ),
        ];
        for (type_name, folder, name, expected) in refusals {
            let error =
                create_standalone_asset(&mut world, &root, type_name, folder, name).unwrap_err();
            assert!(error.contains(expected), "{folder}/{name}: {error}");
        }
        assert_eq!(
            std::fs::read(root.join("notes/taken.note")).unwrap(),
            original,
            "never overwritten"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A sourced test asset with one setting.
    #[derive(Debug)]
    struct Swatch;
    impl pill_engine::Asset for Swatch {}
    trait_type_map::impl_trait_accessible!(dyn pill_engine::Asset; Swatch);

    #[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
    #[serde(default)]
    struct SwatchSettings {
        bright: bool,
    }

    impl pill_engine::ImportedAsset for Swatch {
        type ImportSettings = SwatchSettings;
        const SOURCE_EXTENSIONS: &'static [&'static str] = &["swatch"];

        fn metadata_type_name() -> &'static str {
            "pill_host::asset_browser::tests::Swatch"
        }

        fn import(
            _name: &str,
            _bytes: &[u8],
            _settings: &SwatchSettings,
        ) -> pill_engine::AssetLoadResult<Self> {
            Ok(Self)
        }
    }

    /// Saving settings writes the `.meta` (creating it first when missing),
    /// refuses settings that do not fit, and a move carries the `.meta` along.
    #[test]
    fn settings_are_saved_to_the_metadata_file_and_moves_carry_it() {
        let _mounted = mount_lock();
        let root =
            std::env::temp_dir().join(format!("pill-asset-browser-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.swatch"), b"x").unwrap();
        pill_engine::asset_store::mount_directory(&root);
        let mut world = World::new();
        world.insert_resource(AssetManager::new());
        world.register_imported_asset::<Swatch>();

        let saved = save_asset_settings(
            &mut world,
            &root,
            "a.swatch",
            serde_json::json!({"bright": true}),
        )
        .unwrap();
        assert_eq!(saved, serde_json::json!({"bright": true}));
        let meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(root.join("a.swatch.meta")).unwrap())
                .unwrap();
        assert_eq!(meta["settings"], serde_json::json!({"bright": true}));
        let guid = meta["guid"].clone();
        assert_eq!(
            asset_settings(&world, "a.swatch").unwrap(),
            serde_json::json!({"bright": true})
        );

        // A malformed edit is refused and the file keeps what it had.
        assert!(save_asset_settings(
            &mut world,
            &root,
            "a.swatch",
            serde_json::json!({"bright": "yes"})
        )
        .is_err());
        assert!(
            save_asset_settings(&mut world, &root, "../a.swatch", serde_json::json!({})).is_err()
        );

        // A second save replaces the settings in place and keeps the guid.
        save_asset_settings(&mut world, &root, "a.swatch", serde_json::json!({})).unwrap();
        let meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(root.join("a.swatch.meta")).unwrap())
                .unwrap();
        assert_eq!(meta["settings"], serde_json::json!({"bright": false}));
        assert_eq!(meta["guid"], guid);

        move_asset(&root, "a.swatch", "moved/b.swatch").unwrap();
        assert!(
            root.join("moved/b.swatch").is_file() && root.join("moved/b.swatch.meta").is_file()
        );
        assert!(move_asset(&root, "moved/b.swatch", "../escape.swatch").is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn paths_outside_res_are_refused() {
        assert!(checked_relative("../secret.png").is_err());
        assert!(checked_relative("/abs/a.png").is_err());
        assert!(checked_relative("").is_err());
        assert!(checked_relative("C:\\\\a.png").is_err());
        assert_eq!(
            checked_relative("textures\\\\a.png").unwrap(),
            PathBuf::from("textures/a.png")
        );
    }

    #[test]
    fn the_tree_lists_folders_first_and_hides_metadata() {
        let root = std::env::temp_dir().join(format!("pill-asset-browser-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("textures/nested")).unwrap();
        std::fs::write(root.join("textures/a.png"), b"x").unwrap();
        std::fs::write(
            root.join("textures/a.png.meta"),
            br#"{"guid": "0000000000000000000000000000000a"}"#,
        )
        .unwrap();
        std::fs::write(root.join("textures/nested/b.png"), b"x").unwrap();
        std::fs::write(root.join("readme.txt"), b"x").unwrap();
        std::fs::write(root.join(".hidden"), b"x").unwrap();
        let world = World::new();

        let entries = list_assets(&world, &root);
        let paths: Vec<(&str, usize, bool)> = entries
            .iter()
            .map(|entry| (entry.path.as_str(), entry.depth, entry.is_directory))
            .collect();
        assert_eq!(
            paths,
            [
                ("textures", 0, true),
                ("textures/nested", 1, true),
                ("textures/nested/b.png", 2, false),
                ("textures/a.png", 1, false),
                ("readme.txt", 0, false),
            ]
        );
        let texture = entries
            .iter()
            .find(|entry| entry.path == "textures/a.png")
            .unwrap();
        assert_eq!(
            texture.guid.as_deref(),
            Some("0000000000000000000000000000000a")
        );
        assert!(!texture.loaded, "no registry, so nothing is loaded");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
