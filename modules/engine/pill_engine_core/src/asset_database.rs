//! Every asset of a project by guid and by path, loaded or not:
//! [`AssetDatabase`].
//!
//! # Responsibilities
//!
//! - Index each asset in a project's `res` by its guid and its path, with the
//!   asset type its header names and whether it is standalone
//!   ([`AssetRecord`]): read from the `.meta` sidecars and standalone headers
//!   of a directory ([`AssetDatabase::from_directory`]) or of a shipping pack
//!   ([`AssetDatabase::from_pack`]).
//! - Follow edits on disk one path at a time ([`AssetDatabase::refresh`]):
//!   a new file, a deleted one, and a file moved with its `.meta`.
//! - Report a guid two files claim instead of indexing it twice
//!   ([`AssetDatabase::conflicts`]).
//!
//! # Design
//!
//! The [`AssetManager`](crate::AssetManager) knows the assets that are
//! loaded; this knows the ones that exist. A reference names an asset by guid
//! (see [`AssetReference`](crate::AssetReference)), and resolving one that is
//! not loaded yet needs the step from that guid to a path - the import
//! registry loads by path ([`ImportRegistry::load_by_guid`]).
//!
//! The identity rules are the importer's own (`asset_metadata.rs`): one path is
//! one asset, its guid is the one its `.meta` (or, for a standalone file, its
//! own header) holds, and a guid a second path claims is refused rather than
//! moved. Only the header is read here - `format_version`, `asset_type` and
//! `guid` - never the settings or the asset body, so building the index for a
//! large project costs one small JSON read per asset.
//!
//! A source without a `.meta` has no guid yet and is not in the index; the
//! write that gives it one is a change [`AssetDatabase::refresh`] picks up.
//!
//! [`ImportRegistry::load_by_guid`]: crate::ImportRegistry::load_by_guid

// Standard library
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

// Current crate
use crate::asset::AssetGuid;
use crate::asset_import_registry::ImportRegistry;
use crate::asset_metadata::{METADATA_EXTENSION, METADATA_FORMAT_VERSION};
use crate::asset_store::AssetPack;
use crate::resource::Resource;

// =============================================================================
// AssetRecord
// =============================================================================

/// One asset the project holds, whether or not it is loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetRecord {
    /// The asset's stable identity.
    pub guid: AssetGuid,
    /// Its path relative to `res`, `/`-separated: the name it loads under.
    pub path: String,
    /// The asset type its header names (`pill_master_renderer::assets::Mesh`).
    pub asset_type: String,
    /// Whether the file is the asset itself and holds its own header, rather
    /// than a source with a `.meta` sidecar.
    pub standalone: bool,
}

/// A guid two files claim: the first path keeps it, the second is not indexed.
///
/// The same refusal the importer makes for a source copied together with its
/// `.meta`: the second file needs a guid of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuidConflict {
    /// The contested guid.
    pub guid: AssetGuid,
    /// The path that holds it.
    pub kept: String,
    /// The path that also claims it and was left out.
    pub refused: String,
}

// =============================================================================
// AssetDatabase
// =============================================================================

/// The assets of a project by guid and by path; see the module docs.
///
/// A world resource, inserted by the host when it mounts the project's assets,
/// and read by whatever resolves a reference or lists the assets of a type.
#[derive(Debug, Default, Clone)]
pub struct AssetDatabase {
    /// Every indexed asset, by path, kept sorted so listings are stable.
    records: BTreeMap<String, AssetRecord>,
    /// The path holding each guid.
    by_guid: HashMap<AssetGuid, String>,
    /// Guids a second path claimed, as found by the last build or refresh.
    conflicts: Vec<GuidConflict>,
}

impl Resource for AssetDatabase {}

impl AssetDatabase {
    /// An empty database.
    pub fn new() -> Self {
        Self::default()
    }

    /// Index every asset under `root`, a project's `res` directory.
    ///
    /// `registry` says which extensions are standalone (their file holds the
    /// header); every other file is indexed through its `.meta`, when it has
    /// one. Files are visited in path order, so when two claim one guid the
    /// first path keeps it on every machine.
    pub fn from_directory(root: &Path, registry: &ImportRegistry) -> Self {
        let mut database = Self::new();
        for relative in files_under(root) {
            database.index_from_disk(root, &relative, registry);
        }
        database
    }

    /// Index every asset a shipping pack carries, from the `.meta` entries and
    /// standalone files it holds beside the sources.
    ///
    /// Needs no registry, because a shipping build indexes its pack before any
    /// module has registered a type: an entry with a `.meta` beside it is a
    /// sourced asset, and one without is standalone when the entry itself is
    /// an asset header. A binary source fails that check at its first byte.
    pub fn from_pack(pack: &AssetPack) -> Self {
        let mut database = Self::new();
        let mut paths: Vec<&str> = pack
            .paths()
            .filter(|path| !is_metadata_path(path))
            .collect();
        paths.sort_unstable();
        for path in paths {
            let sidecar = pack
                .get(&format!("{path}.{METADATA_EXTENSION}"))
                .and_then(read_identity);
            let (identity, standalone) = match sidecar {
                Some(identity) => (Some(identity), false),
                None => (pack.get(path).and_then(read_identity), true),
            };
            if let Some((asset_type, guid)) = identity {
                database.insert(AssetRecord {
                    guid,
                    path: path.to_owned(),
                    asset_type,
                    standalone,
                });
            }
        }
        database
    }

    /// Re-read one path under `root` after it changed on disk.
    ///
    /// `relative` is the source's path relative to `root` (a change to a
    /// `.meta` is reported as its source's path). A file that is gone, or no
    /// longer has a header, leaves the index. A file whose guid another path
    /// holds takes it over when that other path no longer exists - a file
    /// moved with its `.meta` - and is refused as a conflict when it does.
    pub fn refresh(&mut self, root: &Path, relative: &str, registry: &ImportRegistry) {
        let path = slash_path(relative);
        self.remove_path(&path);
        self.conflicts.retain(|conflict| conflict.refused != path);
        if !root.join(&path).is_file() {
            return;
        }
        let Some(record) = read_record(root, &path, registry) else {
            return;
        };
        // The guid's current owner is gone from disk: this is where it moved.
        if let Some(owner) = self.by_guid.get(&record.guid).cloned() {
            if !root.join(&owner).is_file() {
                self.remove_path(&owner);
            }
        }
        self.insert(record);
    }

    /// The asset with `guid`, if one is indexed.
    pub fn by_guid(&self, guid: AssetGuid) -> Option<&AssetRecord> {
        self.records.get(self.by_guid.get(&guid)?)
    }

    /// The asset at `path` (relative to `res`, either separator), if indexed.
    pub fn by_path(&self, path: &str) -> Option<&AssetRecord> {
        self.records.get(&slash_path(path))
    }

    /// Every indexed asset, in path order.
    pub fn records(&self) -> impl Iterator<Item = &AssetRecord> {
        self.records.values()
    }

    /// Every indexed asset of the type `asset_type`, in path order: what a
    /// reference picker lists.
    pub fn records_of_type<'a>(
        &'a self,
        asset_type: &'a str,
    ) -> impl Iterator<Item = &'a AssetRecord> + 'a {
        self.records
            .values()
            .filter(move |record| record.asset_type == asset_type)
    }

    /// The guids a second path claimed, as of the last build or refresh.
    pub fn conflicts(&self) -> &[GuidConflict] {
        &self.conflicts
    }

    /// How many assets are indexed.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether nothing is indexed.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Index `relative` from the files under `root`, if it carries a header.
    fn index_from_disk(&mut self, root: &Path, relative: &str, registry: &ImportRegistry) {
        if let Some(record) = read_record(root, relative, registry) {
            self.insert(record);
        }
    }

    /// Add `record`, refusing it as a conflict when another path holds its guid.
    fn insert(&mut self, record: AssetRecord) {
        if let Some(owner) = self.by_guid.get(&record.guid) {
            if owner != &record.path {
                self.conflicts.push(GuidConflict {
                    guid: record.guid,
                    kept: owner.clone(),
                    refused: record.path,
                });
                return;
            }
        }
        self.by_guid.insert(record.guid, record.path.clone());
        self.records.insert(record.path.clone(), record);
    }

    /// Drop the record at `path`, if any, and its guid entry.
    fn remove_path(&mut self, path: &str) {
        if let Some(record) = self.records.remove(path) {
            if self
                .by_guid
                .get(&record.guid)
                .is_some_and(|owner| owner == path)
            {
                self.by_guid.remove(&record.guid);
            }
        }
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// The record for `relative` under `root`: its header read from the file
/// itself when standalone, from its `.meta` otherwise; `None` without one.
fn read_record(root: &Path, relative: &str, registry: &ImportRegistry) -> Option<AssetRecord> {
    let standalone = is_standalone(relative, registry);
    let header_path = if standalone {
        root.join(relative)
    } else {
        root.join(format!("{relative}.{METADATA_EXTENSION}"))
    };
    let bytes = std::fs::read(header_path).ok()?;
    let (asset_type, guid) = read_identity(&bytes)?;
    Some(AssetRecord {
        guid,
        path: slash_path(relative),
        asset_type,
        standalone,
    })
}

/// The `asset_type` and `guid` of a header, when it is one this engine reads.
///
/// The same three checks the importer's header parser makes - a supported
/// `format_version`, an `asset_type`, a 32-digit guid - without requiring an
/// expected type, since the index is built before anything is imported.
fn read_identity(bytes: &[u8]) -> Option<(String, AssetGuid)> {
    let document: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let header = document.as_object()?;
    let version = header.get("format_version")?.as_u64()?;
    if version == 0 || version > u64::from(METADATA_FORMAT_VERSION) {
        return None;
    }
    let asset_type = header.get("asset_type")?.as_str()?.to_owned();
    let guid = AssetGuid::parse(header.get("guid")?.as_str()?)?;
    Some((asset_type, guid))
}

/// Whether `path`'s extension is a standalone type's.
fn is_standalone(path: &str, registry: &ImportRegistry) -> bool {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| registry.is_standalone_extension(extension))
}

/// Whether `path` names a `.meta` sidecar.
fn is_metadata_path(path: &str) -> bool {
    Path::new(path)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case(METADATA_EXTENSION))
}

/// `path` with `/` separators, the form the index keys on.
fn slash_path(path: &str) -> String {
    path.replace('\\', "/")
}

/// Every file under `root` as a `/`-separated relative path, sorted; `.meta`
/// files and dot-files left out. An unreadable directory contributes nothing.
fn files_under(root: &Path) -> Vec<String> {
    let mut files = Vec::new();
    let mut pending = vec![String::new()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(root.join(&directory)) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let relative = if directory.is_empty() {
                name
            } else {
                format!("{directory}/{name}")
            };
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => pending.push(relative),
                Ok(kind) if kind.is_file() && !is_metadata_path(&relative) => files.push(relative),
                _ => {}
            }
        }
    }
    files.sort();
    files
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::{Asset, AssetLoadResult, AssetManager};
    use crate::asset_metadata::{ImportedAsset, MetadataPolicy};
    use crate::asset_standalone::StandaloneAsset;
    use crate::asset_store::{mount_directory, mounted_directory_test_lock};
    use crate::World;
    use serde::{Deserialize, Serialize};
    use std::path::PathBuf;
    use trait_type_map::impl_trait_accessible;

    /// An imported test asset: a `.note` file's text.
    #[derive(Debug)]
    struct Note(String);
    impl Asset for Note {}
    impl_trait_accessible!(dyn Asset; Note);

    impl ImportedAsset for Note {
        type ImportSettings = NoteSettings;
        const SOURCE_EXTENSIONS: &'static [&'static str] = &["note"];

        fn metadata_type_name() -> &'static str {
            "pill_engine::asset_database::tests::Note"
        }

        fn import(_name: &str, bytes: &[u8], _settings: &NoteSettings) -> AssetLoadResult<Self> {
            Ok(Self(String::from_utf8_lossy(bytes).into_owned()))
        }
    }

    /// The note's (empty) import settings.
    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    struct NoteSettings {}

    /// A standalone test asset: a `.card` file is the asset.
    #[derive(Debug)]
    struct Card;
    impl Asset for Card {}
    impl_trait_accessible!(dyn Asset; Card);

    impl StandaloneAsset for Card {
        type Document = CardDocument;
        const FILE_EXTENSION: &'static str = "card";

        fn metadata_type_name() -> &'static str {
            "pill_engine::asset_database::tests::Card"
        }

        fn from_document(
            _name: &str,
            _document: CardDocument,
            _assets: &AssetManager,
        ) -> AssetLoadResult<Self> {
            Ok(Self)
        }
    }

    /// The card's (empty) document.
    #[derive(Debug, Default, Serialize, Deserialize)]
    struct CardDocument {}

    const NOTE_GUID: &str = "000000000000000000000000000000a1";
    const CARD_GUID: &str = "000000000000000000000000000000c1";

    /// A scratch `res` directory, mounted while the test holds the lock, and a
    /// world with both test types registered.
    struct Scratch {
        root: PathBuf,
        world: World,
        _mounted: std::sync::MutexGuard<'static, ()>,
    }

    impl Scratch {
        fn new(label: &str) -> Self {
            let mounted = mounted_directory_test_lock();
            let root = std::env::temp_dir().join(format!(
                "pill-asset-database-{label}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            mount_directory(&root);
            let mut world = World::new();
            world.register_imported_asset::<Note>();
            world.register_standalone_asset::<Card>();
            Self {
                root,
                world,
                _mounted: mounted,
            }
        }

        fn write(&self, relative: &str, text: &str) {
            let path = self.root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }

        /// A note at `relative` with a `.meta` holding `guid`.
        fn note(&self, relative: &str, guid: &str) {
            self.write(relative, "hello");
            self.write(
                &format!("{relative}.meta"),
                &header("pill_engine::asset_database::tests::Note", guid, "settings"),
            );
        }

        fn registry(&self) -> ImportRegistry {
            self.world.get_resource::<ImportRegistry>().unwrap().clone()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// A header document naming `asset_type` and `guid`, with an empty body
    /// under `body_field`.
    fn header(asset_type: &str, guid: &str, body_field: &str) -> String {
        format!(
            r#"{{"format_version": 1, "asset_type": "{asset_type}", "guid": "{guid}", "{body_field}": {{}}}}"#
        )
    }

    fn guid(text: &str) -> AssetGuid {
        AssetGuid::parse(text).unwrap()
    }

    #[test]
    fn a_directory_indexes_sourced_and_standalone_assets() {
        let scratch = Scratch::new("index");
        scratch.note("notes/a.note", NOTE_GUID);
        scratch.write(
            "cards/b.card",
            &header(
                "pill_engine::asset_database::tests::Card",
                CARD_GUID,
                "asset",
            ),
        );
        // A source without a `.meta` has no guid yet, and a `.meta` is never
        // an asset of its own.
        scratch.write("notes/unpaired.note", "no sidecar");

        let database = AssetDatabase::from_directory(&scratch.root, &scratch.registry());
        assert_eq!(database.len(), 2);
        let note = database
            .by_guid(guid(NOTE_GUID))
            .expect("the note is indexed");
        assert_eq!(note.path, "notes/a.note");
        assert_eq!(note.asset_type, "pill_engine::asset_database::tests::Note");
        assert!(!note.standalone);
        let card = database
            .by_path("cards\\b.card")
            .expect("either separator works");
        assert_eq!(card.guid, guid(CARD_GUID));
        assert!(card.standalone);
        assert_eq!(
            database
                .records_of_type("pill_engine::asset_database::tests::Card")
                .count(),
            1
        );
    }

    #[test]
    fn a_guid_claimed_twice_keeps_the_first_path() {
        let scratch = Scratch::new("duplicate");
        scratch.note("a.note", NOTE_GUID);
        scratch.note("b.note", NOTE_GUID);

        let database = AssetDatabase::from_directory(&scratch.root, &scratch.registry());
        assert_eq!(database.by_guid(guid(NOTE_GUID)).unwrap().path, "a.note");
        assert!(database.by_path("b.note").is_none());
        assert_eq!(
            database.conflicts(),
            &[GuidConflict {
                guid: guid(NOTE_GUID),
                kept: "a.note".to_owned(),
                refused: "b.note".to_owned(),
            }]
        );
    }

    #[test]
    fn refresh_follows_a_new_file_a_move_and_a_delete() {
        let scratch = Scratch::new("refresh");
        let registry = scratch.registry();
        let mut database = AssetDatabase::from_directory(&scratch.root, &registry);
        assert!(database.is_empty());

        // New: a note and its `.meta` appear.
        scratch.note("a.note", NOTE_GUID);
        database.refresh(&scratch.root, "a.note", &registry);
        assert_eq!(database.by_guid(guid(NOTE_GUID)).unwrap().path, "a.note");

        // Moved with its `.meta`: the new path arrives first, as the watcher
        // reports it, and takes the guid over because the old one is gone.
        std::fs::create_dir_all(scratch.root.join("moved")).unwrap();
        std::fs::rename(
            scratch.root.join("a.note"),
            scratch.root.join("moved/a.note"),
        )
        .unwrap();
        std::fs::rename(
            scratch.root.join("a.note.meta"),
            scratch.root.join("moved/a.note.meta"),
        )
        .unwrap();
        database.refresh(&scratch.root, "moved/a.note", &registry);
        database.refresh(&scratch.root, "a.note", &registry);
        assert_eq!(
            database.by_guid(guid(NOTE_GUID)).unwrap().path,
            "moved/a.note"
        );
        assert!(database.by_path("a.note").is_none());
        assert!(database.conflicts().is_empty());

        // Deleted: it leaves the index.
        std::fs::remove_file(scratch.root.join("moved/a.note")).unwrap();
        database.refresh(&scratch.root, "moved/a.note", &registry);
        assert!(database.by_guid(guid(NOTE_GUID)).is_none());
    }

    #[test]
    fn a_copy_with_its_meta_is_refused_while_the_original_exists() {
        let scratch = Scratch::new("copy");
        let registry = scratch.registry();
        scratch.note("a.note", NOTE_GUID);
        let mut database = AssetDatabase::from_directory(&scratch.root, &registry);
        scratch.note("copy.note", NOTE_GUID);
        database.refresh(&scratch.root, "copy.note", &registry);
        assert_eq!(database.by_guid(guid(NOTE_GUID)).unwrap().path, "a.note");
        assert_eq!(database.conflicts().len(), 1);
    }

    #[test]
    fn an_unloaded_asset_loads_by_guid_and_by_path_to_the_same_handle() {
        let mut scratch = Scratch::new("load");
        scratch.note("a.note", NOTE_GUID);
        let registry = scratch.registry();
        let database = AssetDatabase::from_directory(&scratch.root, &registry);
        let assets = scratch.world.get_resource_mut::<AssetManager>().unwrap();
        assert!(assets.handle_by_guid::<Note>(guid(NOTE_GUID)).is_none());

        let by_guid = registry
            .load_by_guid(
                assets,
                &database,
                guid(NOTE_GUID),
                MetadataPolicy::ReadIfPresent,
            )
            .expect("the guid resolves to a path and imports");
        assert!(!by_guid.previously_loaded);
        let by_path = registry
            .load_by_path(assets, Path::new("a.note"), MetadataPolicy::ReadIfPresent)
            .expect("the path imports");
        assert!(by_path.previously_loaded);
        assert_eq!(
            (by_guid.index, by_guid.generation),
            (by_path.index, by_path.generation)
        );
        let handle = registry
            .load_handle_by_guid::<Note>(
                assets,
                &database,
                guid(NOTE_GUID),
                MetadataPolicy::ReadIfPresent,
            )
            .expect("the typed form finds the same asset");
        assert_eq!(
            (handle.index(), handle.generation()),
            (by_path.index, by_path.generation)
        );
        assert_eq!(
            assets.get(handle).map(|note| note.0.as_str()),
            Some("hello")
        );
    }

    #[test]
    fn an_unknown_guid_or_a_wrong_type_is_an_error() {
        let mut scratch = Scratch::new("errors");
        scratch.note("a.note", NOTE_GUID);
        let registry = scratch.registry();
        let database = AssetDatabase::from_directory(&scratch.root, &registry);
        let assets = scratch.world.get_resource_mut::<AssetManager>().unwrap();
        let unknown = registry.load_by_guid(
            assets,
            &database,
            guid(CARD_GUID),
            MetadataPolicy::ReadIfPresent,
        );
        assert!(matches!(
            unknown,
            Err(crate::ErasedImportError::UnknownGuid { .. })
        ));
        let wrong_type = registry.load_handle_by_guid::<Card>(
            assets,
            &database,
            guid(NOTE_GUID),
            MetadataPolicy::ReadIfPresent,
        );
        assert!(matches!(
            wrong_type,
            Err(crate::ErasedImportError::WrongType { .. })
        ));
    }

    #[test]
    fn a_pack_indexes_its_meta_entries() {
        let pack_bytes = pack(&[
            ("a.note", b"hello".to_vec()),
            (
                "a.note.meta",
                header(
                    "pill_engine::asset_database::tests::Note",
                    NOTE_GUID,
                    "settings",
                )
                .into_bytes(),
            ),
            ("loose.txt", b"not an asset".to_vec()),
            (
                "b.card",
                header(
                    "pill_engine::asset_database::tests::Card",
                    CARD_GUID,
                    "asset",
                )
                .into_bytes(),
            ),
        ]);
        let pack = AssetPack::parse(pack_bytes).unwrap();
        let database = AssetDatabase::from_pack(&pack);
        assert_eq!(
            database.len(),
            2,
            "the note and the standalone card; the loose text is not an asset"
        );
        let note = database.by_guid(guid(NOTE_GUID)).unwrap();
        assert_eq!((note.path.as_str(), note.standalone), ("a.note", false));
        let card = database.by_guid(guid(CARD_GUID)).unwrap();
        assert_eq!((card.path.as_str(), card.standalone), ("b.card", true));
    }

    /// The bytes of a version-1 pack holding `files`, as the pack writer lays
    /// them out (see `asset_store.rs`'s format notes).
    fn pack(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut index_length = 0usize;
        for (path, _) in files {
            index_length += 4 + path.len() + 16;
        }
        let header_length = crate::asset_store::ASSET_PACK_MAGIC.len() + 8;
        let mut offset = header_length + index_length;
        let mut bytes = crate::asset_store::ASSET_PACK_MAGIC.to_vec();
        bytes.extend_from_slice(&crate::asset_store::ASSET_PACK_VERSION.to_le_bytes());
        bytes.extend_from_slice(&(files.len() as u32).to_le_bytes());
        for (path, content) in files {
            bytes.extend_from_slice(&(path.len() as u32).to_le_bytes());
            bytes.extend_from_slice(path.as_bytes());
            bytes.extend_from_slice(&(offset as u64).to_le_bytes());
            bytes.extend_from_slice(&(content.len() as u64).to_le_bytes());
            offset += content.len();
        }
        for (_, content) in files {
            bytes.extend_from_slice(content);
        }
        bytes
    }
}
