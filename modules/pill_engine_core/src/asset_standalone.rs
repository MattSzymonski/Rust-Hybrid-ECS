//! Assets with no source file: the file in `res` is the asset itself.
//!
//! # Responsibilities
//!
//! - Define what such an asset type provides ([`StandaloneAsset`]): its
//!   serialized document, the one file extension it is stored under, and how
//!   to build the asset from a document.
//! - Load, reload in place and follow a move of a standalone file
//!   ([`AssetManager::import_standalone`] and its siblings), with the same
//!   identity rules as a sourced asset: named by its path, keyed by its guid.
//! - Write a new standalone file ([`render_standalone`]), and read or check
//!   its document as JSON for code that does not know its type.
//!
//! # Design
//!
//! A material or a render pass has no image or model behind it: it is data.
//! Its file therefore carries the same header a `.meta` file does - format
//! version, asset type, guid - with the asset itself under `asset` where a
//! metadata file has `settings`:
//!
//! ```json
//! {
//!   "format_version": 1,
//!   "asset_type": "pill_master_renderer::assets::Material",
//!   "guid": "5d1f0e2b9a8c4d3e7f6a5b4c3d2e1f00",
//!   "asset": { "shader": "...", "parameters": { "pbr_roughness": { "Scalar": 1.0 } } }
//! }
//! ```
//!
//! The guid lives in the file's own header, so a standalone file has no
//! `.meta`, and `.meta` stays a sidecar only. Its type is found by its
//! extension (`helmet.material`), exactly like a sourced asset's, so the scan,
//! the watcher and the move-following of the earlier stages treat it with no
//! special case.
//!
//! A document refers to other assets by
//! [`AssetReference`](crate::asset_reference::AssetReference), resolved when
//! the asset is built. A reference to an asset that is not loaded yet resolves
//! to the invalid handle, which is why a scan imports standalone files after
//! every sourced one.

// Standard library
use std::path::Path;

// External crates
use serde::de::DeserializeOwned;
use serde::Serialize;
use trait_type_map::TraitAccessible;

// Current crate
use crate::asset::{Asset, AssetGuid, AssetLoadError, AssetLoadResult, AssetManager};
use crate::asset_metadata::{
    parse_header, AssetImportError, ImportOutcome, MetadataSource, ReimportOutcome,
    METADATA_FORMAT_VERSION,
};
use crate::asset_store::{self, pack_key};

/// The header field holding a standalone file's asset.
pub const STANDALONE_BODY_FIELD: &str = "asset";

/// An asset stored as its own JSON file in `res`, with no source file.
///
/// Implemented by the crate that owns the type, and registered with
/// [`World::register_standalone_asset`](crate::World::register_standalone_asset)
/// so it can be loaded by extension.
pub trait StandaloneAsset: Asset + TraitAccessible<dyn Asset> + Sized {
    /// The asset as written in its file. Its `Default` is what a newly
    /// created file holds, so it must build a valid asset on its own. Mark it
    /// `#[serde(default)]` so a file written before a field existed still
    /// reads, and use `BTreeMap`, never `HashMap`, for maps (see
    /// `ImportedAsset::ImportSettings`).
    type Document: Serialize + DeserializeOwned + Default;

    /// The file extension the type is stored under, lowercase and without the
    /// dot (`"material"`).
    const FILE_EXTENSION: &'static str;

    /// The stable name written to the file's `asset_type`. Defaults to
    /// [`Asset::shared_name`]; a type without one should override it with a
    /// pinned literal.
    fn metadata_type_name() -> &'static str {
        Self::shared_name().unwrap_or_else(std::any::type_name::<Self>)
    }

    /// Build the asset named `name` from its document, resolving its
    /// references against `assets`.
    ///
    /// # Errors
    ///
    /// Returns an [`AssetLoadError`] when the document cannot make a valid
    /// asset.
    fn from_document(
        name: &str,
        document: Self::Document,
        assets: &AssetManager,
    ) -> AssetLoadResult<Self>;
}

/// The bytes of a standalone file for `T` holding `guid` and `document`.
///
/// # Errors
///
/// Returns [`AssetLoadError::Metadata`] when the document cannot be written as
/// JSON.
pub fn render_standalone<T: StandaloneAsset>(
    guid: AssetGuid,
    document: &T::Document,
) -> AssetLoadResult<Vec<u8>> {
    let value = serde_json::json!({
        "format_version": METADATA_FORMAT_VERSION,
        "asset_type": T::metadata_type_name(),
        "guid": guid,
        STANDALONE_BODY_FIELD: document,
    });
    let mut bytes =
        serde_json::to_vec_pretty(&value).map_err(|error| AssetLoadError::Metadata {
            path: T::metadata_type_name().into(),
            detail: format!("the document cannot be written as JSON: {error}"),
        })?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Read the standalone file at the normalized `name` as `T`'s: its guid and
/// document.
fn read_standalone<T: StandaloneAsset>(name: &str) -> AssetLoadResult<(AssetGuid, T::Document)> {
    let path = Path::new(name);
    let bytes = asset_store::read(path)?;
    let (guid, body) = parse_header(path, &bytes, T::metadata_type_name(), STANDALONE_BODY_FIELD)?;
    let document = match body {
        None => T::Document::default(),
        Some(body) => serde_json::from_value(body).map_err(|error| AssetLoadError::Metadata {
            path: path.to_owned(),
            detail: format!("`{STANDALONE_BODY_FIELD}` does not fit this asset type: {error}"),
        })?,
    };
    Ok((guid, document))
}

/// `path` normalized into an asset name, or the refusal `import` gives.
fn asset_name_of(path: &Path) -> Result<String, AssetLoadError> {
    pack_key(path).ok_or_else(|| AssetLoadError::PathNotFound {
        path: path.to_owned(),
    })
}

impl AssetManager {
    /// Load the standalone file at `path` as a `T`.
    ///
    /// Named by its normalized path and keyed by the guid in its header, under
    /// the same rules as [`Self::import`]: a path already loaded under that
    /// guid returns its handle with `already_loaded` set, and nothing is
    /// decoded; a guid another live `T` has is refused. Nothing is ever
    /// written: the file is the asset.
    ///
    /// # Errors
    ///
    /// [`AssetImportError::Load`] when the file cannot be read, is not a `T`'s
    /// file, or does not build; the guid conflicts as in [`Self::import`].
    pub fn import_standalone<T: StandaloneAsset>(
        &mut self,
        path: &Path,
    ) -> Result<ImportOutcome<T>, AssetImportError> {
        let asset_name = asset_name_of(path)?;
        let (guid, document) = read_standalone::<T>(&asset_name)?;

        if let Some(handle) = self.handle_by_name::<T>(&asset_name) {
            let loaded = self.guid_of(handle);
            if loaded == Some(guid) {
                return Ok(ImportOutcome {
                    handle,
                    guid,
                    metadata: MetadataSource::ReadFromFile,
                    already_loaded: true,
                });
            }
            return Err(AssetImportError::PathBoundToOtherGuid {
                path: asset_name,
                loaded,
                metadata: guid,
            });
        }
        if let Some(other) = self.handle_by_guid::<T>(guid) {
            return Err(AssetImportError::DuplicateGuid {
                guid,
                path: asset_name,
                other_path: self.name_of(other).unwrap_or("<unnamed>").to_owned(),
            });
        }

        let asset = T::from_document(&asset_name, document, self)?;
        let handle = self
            .add_named_with_guid(asset_name, guid, asset)
            .expect("the path was checked to be unloaded before building");
        Ok(ImportOutcome {
            handle,
            guid,
            metadata: MetadataSource::ReadFromFile,
            already_loaded: false,
        })
    }

    /// Read the standalone file at `path` again and replace the loaded value
    /// in its slot, keeping its handle, name and guid; a path that is not
    /// loaded is imported instead.
    ///
    /// # Errors
    ///
    /// As [`Self::import_standalone`]; a header whose guid changed under the
    /// loaded asset is [`AssetImportError::PathBoundToOtherGuid`]. On every
    /// error the loaded value stays as it was.
    pub fn reimport_standalone<T: StandaloneAsset>(
        &mut self,
        path: &Path,
    ) -> Result<ReimportOutcome<T>, AssetImportError> {
        let asset_name = asset_name_of(path)?;
        let Some(handle) = self.handle_by_name::<T>(&asset_name) else {
            let imported = self.import_standalone::<T>(path)?;
            return Ok(ReimportOutcome {
                handle: imported.handle,
                guid: imported.guid,
                metadata: imported.metadata,
                replaced: false,
            });
        };
        let (guid, document) = read_standalone::<T>(&asset_name)?;
        let loaded = self.guid_of(handle);
        if loaded != Some(guid) {
            return Err(AssetImportError::PathBoundToOtherGuid {
                path: asset_name,
                loaded,
                metadata: guid,
            });
        }
        // Built fully before the slot is touched, so a failure leaves it as is.
        let asset = T::from_document(&asset_name, document, self)?;
        *self
            .get_mut(handle)
            .expect("the handle was looked up by name just above") = asset;
        Ok(ReimportOutcome {
            handle,
            guid,
            metadata: MetadataSource::ReadFromFile,
            replaced: true,
        })
    }

    /// When the standalone file at `path` is a loaded `T` moved here, rename
    /// that asset to `path` and return its old name; the counterpart of
    /// [`Self::follow_move`], recognizing the move by the guid in the file's
    /// own header.
    ///
    /// # Errors
    ///
    /// When the path does not normalize, or the file exists but is not a
    /// readable `T` file.
    pub fn follow_standalone_move<T: StandaloneAsset>(
        &mut self,
        path: &Path,
    ) -> Result<Option<String>, AssetImportError> {
        let asset_name = asset_name_of(path)?;
        if self.handle_by_name::<T>(&asset_name).is_some() {
            return Ok(None);
        }
        let (guid, _) = match read_standalone::<T>(&asset_name) {
            Ok(read) => read,
            Err(AssetLoadError::PathNotFound { .. }) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let Some(handle) = self.handle_by_guid::<T>(guid) else {
            return Ok(None);
        };
        let Some(old_name) = self.name_of(handle).map(str::to_owned) else {
            return Ok(None);
        };
        if asset_store::locate_file(Path::new(&old_name)).is_some() {
            return Ok(None);
        }
        self.rename(handle, asset_name)
            .map_err(|error| AssetLoadError::Metadata {
                path: path.to_owned(),
                detail: format!("the moved asset could not take its new name: {error}"),
            })?;
        Ok(Some(old_name))
    }
}

/// The document in the standalone file at `path`, as JSON.
///
/// # Errors
///
/// When the file cannot be read or is not a `T`'s.
pub fn standalone_document_json<T: StandaloneAsset>(
    path: &Path,
) -> AssetLoadResult<serde_json::Value> {
    let asset_name = asset_name_of(path)?;
    let (_, document) = read_standalone::<T>(&asset_name)?;
    document_to_json::<T>(path, &document)
}

/// `value` read as `T`'s document and written back: missing fields defaulted,
/// unknown ones dropped.
///
/// # Errors
///
/// [`AssetLoadError::Metadata`] when the value does not fit the document.
pub fn normalize_standalone_document<T: StandaloneAsset>(
    value: serde_json::Value,
) -> AssetLoadResult<serde_json::Value> {
    let label = Path::new(T::metadata_type_name());
    let document: T::Document =
        serde_json::from_value(value).map_err(|error| AssetLoadError::Metadata {
            path: label.to_owned(),
            detail: format!("the document does not fit this asset type: {error}"),
        })?;
    document_to_json::<T>(label, &document)
}

/// The document a new `T` file holds, as JSON: `T::Document::default()`.
///
/// # Errors
///
/// When the default document cannot be written as JSON.
pub fn default_standalone_document<T: StandaloneAsset>() -> AssetLoadResult<serde_json::Value> {
    document_to_json::<T>(Path::new(T::metadata_type_name()), &T::Document::default())
}

/// `document` as JSON, with `path` naming the failure.
fn document_to_json<T: StandaloneAsset>(
    path: &Path,
    document: &T::Document,
) -> AssetLoadResult<serde_json::Value> {
    serde_json::to_value(document).map_err(|error| AssetLoadError::Metadata {
        path: path.to_owned(),
        detail: format!("the document cannot be written as JSON: {error}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::Handle;
    use crate::asset_reference::AssetReference;
    use crate::asset_store::{mount_directory, mounted_directory_test_lock};
    use serde::Deserialize;
    use std::path::PathBuf;
    use trait_type_map::impl_trait_accessible;

    /// A sourceless picture the tests refer to by guid.
    #[derive(Debug, PartialEq)]
    struct Picture;
    impl Asset for Picture {}
    impl_trait_accessible!(dyn Asset; Picture);

    /// The standalone test asset: a tint and a picture reference.
    #[derive(Debug)]
    struct Swatch {
        name: String,
        tint: u32,
        picture: Handle<Picture>,
    }
    impl Asset for Swatch {}
    impl_trait_accessible!(dyn Asset; Swatch);

    #[derive(Serialize, Deserialize)]
    #[serde(default)]
    struct SwatchDocument {
        tint: u32,
        picture: AssetReference<Picture>,
    }

    impl Default for SwatchDocument {
        fn default() -> Self {
            Self {
                tint: 5,
                picture: AssetReference::unset(),
            }
        }
    }

    impl StandaloneAsset for Swatch {
        type Document = SwatchDocument;
        const FILE_EXTENSION: &'static str = "swatch";

        fn metadata_type_name() -> &'static str {
            "pill_engine::asset_standalone::tests::Swatch"
        }

        fn from_document(
            name: &str,
            document: SwatchDocument,
            assets: &AssetManager,
        ) -> AssetLoadResult<Self> {
            if document.tint > 100 {
                return Err(AssetLoadError::Decode {
                    label: name.to_owned(),
                    detail: "tint above 100".to_owned(),
                });
            }
            Ok(Self {
                name: name.to_owned(),
                tint: document.tint,
                picture: document.picture.resolve(assets),
            })
        }
    }

    const GUID: &str = "0000000000000000000000000000005a";
    const PICTURE_GUID: &str = "000000000000000000000000000000b1";

    /// A scratch `res` directory mounted under the mounted-directory lock.
    struct ScratchRes {
        root: PathBuf,
        _mounted: std::sync::MutexGuard<'static, ()>,
    }

    impl ScratchRes {
        fn new(label: &str) -> Self {
            let mounted = mounted_directory_test_lock();
            let root = std::env::temp_dir()
                .join(format!("pill-standalone-{label}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            mount_directory(&root);
            Self {
                root,
                _mounted: mounted,
            }
        }

        fn write_swatch(&self, relative: &str, guid: &str, body: &str) {
            let path = self.root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                path,
                format!(
                    r#"{{"format_version": 1, "asset_type": "{}", "guid": "{guid}", "asset": {body}}}"#,
                    Swatch::metadata_type_name()
                ),
            )
            .unwrap();
        }
    }

    impl Drop for ScratchRes {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn with_picture(assets: &mut AssetManager) -> Handle<Picture> {
        assets
            .add_named_with_guid("picture", AssetGuid::parse(PICTURE_GUID).unwrap(), Picture)
            .unwrap()
    }

    #[test]
    fn a_standalone_file_loads_with_its_references_resolved() {
        let res = ScratchRes::new("load");
        res.write_swatch(
            "swatches/a.swatch",
            GUID,
            &format!(r#"{{"tint": 9, "picture": "{PICTURE_GUID}"}}"#),
        );
        let mut assets = AssetManager::new();
        let picture = with_picture(&mut assets);

        let outcome = assets
            .import_standalone::<Swatch>(Path::new("swatches/a.swatch"))
            .unwrap();

        let swatch = assets.get(outcome.handle).unwrap();
        assert_eq!(swatch.name, "swatches/a.swatch");
        assert_eq!(swatch.tint, 9);
        assert_eq!(swatch.picture, picture);
        assert_eq!(outcome.guid, AssetGuid::parse(GUID).unwrap());
        assert_eq!(outcome.metadata, MetadataSource::ReadFromFile);
        assert!(
            !res.root.join("swatches/a.swatch.meta").exists(),
            "no sidecar"
        );
    }

    #[test]
    fn importing_again_returns_the_handle_and_a_missing_body_is_the_default() {
        let res = ScratchRes::new("again");
        res.write_swatch("a.swatch", GUID, "null");
        let mut assets = AssetManager::new();

        let first = assets
            .import_standalone::<Swatch>(Path::new("a.swatch"))
            .unwrap();
        let second = assets
            .import_standalone::<Swatch>(Path::new("a.swatch"))
            .unwrap();

        assert!(second.already_loaded);
        assert_eq!(second.handle, first.handle);
        assert_eq!(assets.get(first.handle).unwrap().tint, 5);
        assert_eq!(assets.get(first.handle).unwrap().picture, Handle::INVALID);
    }

    #[test]
    fn a_wrong_type_or_a_copied_guid_is_refused() {
        let res = ScratchRes::new("refused");
        res.write_swatch("a.swatch", GUID, "{}");
        res.write_swatch("copy.swatch", GUID, "{}");
        std::fs::write(
            res.root.join("other.swatch"),
            format!(
                r#"{{"format_version": 1, "asset_type": "something::Else", "guid": "{GUID}"}}"#
            ),
        )
        .unwrap();
        let mut assets = AssetManager::new();
        assets
            .import_standalone::<Swatch>(Path::new("a.swatch"))
            .unwrap();

        assert!(matches!(
            assets.import_standalone::<Swatch>(Path::new("copy.swatch")),
            Err(AssetImportError::DuplicateGuid { .. })
        ));
        assert!(matches!(
            assets.import_standalone::<Swatch>(Path::new("other.swatch")),
            Err(AssetImportError::Load(AssetLoadError::Metadata { .. }))
        ));
    }

    #[test]
    fn a_reimport_replaces_the_slot_and_a_bad_edit_keeps_it() {
        let res = ScratchRes::new("reimport");
        res.write_swatch("a.swatch", GUID, r#"{"tint": 1}"#);
        let mut assets = AssetManager::new();
        let first = assets
            .import_standalone::<Swatch>(Path::new("a.swatch"))
            .unwrap();
        let version = assets.content_version(first.handle).unwrap();

        res.write_swatch("a.swatch", GUID, r#"{"tint": 2}"#);
        let outcome = assets
            .reimport_standalone::<Swatch>(Path::new("a.swatch"))
            .unwrap();
        assert!(outcome.replaced);
        assert_eq!(outcome.handle, first.handle);
        assert_eq!(assets.get(first.handle).unwrap().tint, 2);
        assert!(assets.content_version(first.handle).unwrap() > version);

        res.write_swatch("a.swatch", GUID, r#"{"tint": 500}"#);
        assert!(assets
            .reimport_standalone::<Swatch>(Path::new("a.swatch"))
            .is_err());
        assert_eq!(assets.get(first.handle).unwrap().tint, 2);
    }

    #[test]
    fn a_moved_standalone_file_is_followed() {
        let res = ScratchRes::new("move");
        res.write_swatch("a.swatch", GUID, "{}");
        let mut assets = AssetManager::new();
        let first = assets
            .import_standalone::<Swatch>(Path::new("a.swatch"))
            .unwrap();

        std::fs::create_dir_all(res.root.join("moved")).unwrap();
        std::fs::rename(res.root.join("a.swatch"), res.root.join("moved/a.swatch")).unwrap();
        let old = assets
            .follow_standalone_move::<Swatch>(Path::new("moved/a.swatch"))
            .unwrap();

        assert_eq!(old.as_deref(), Some("a.swatch"));
        assert_eq!(
            assets.handle_by_name::<Swatch>("moved/a.swatch"),
            Some(first.handle)
        );
    }

    #[test]
    fn a_rendered_file_reads_back_and_documents_go_through_json() {
        let res = ScratchRes::new("render");
        let guid = AssetGuid::parse(GUID).unwrap();
        let bytes = render_standalone::<Swatch>(guid, &SwatchDocument::default()).unwrap();
        std::fs::write(res.root.join("new.swatch"), bytes).unwrap();
        let mut assets = AssetManager::new();

        let outcome = assets
            .import_standalone::<Swatch>(Path::new("new.swatch"))
            .unwrap();
        assert_eq!(outcome.guid, guid);
        assert_eq!(
            standalone_document_json::<Swatch>(Path::new("new.swatch")).unwrap(),
            serde_json::json!({"tint": 5, "picture": null})
        );
        assert_eq!(
            default_standalone_document::<Swatch>().unwrap(),
            serde_json::json!({"tint": 5, "picture": null})
        );
        assert_eq!(
            normalize_standalone_document::<Swatch>(serde_json::json!({"tint": 7, "x": 1}))
                .unwrap(),
            serde_json::json!({"tint": 7, "picture": null})
        );
    }
}
