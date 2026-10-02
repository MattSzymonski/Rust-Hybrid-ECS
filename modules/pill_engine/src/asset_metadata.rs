//! Metadata files beside an asset's source, and loading an asset through one.
//!
//! # Responsibilities
//!
//! - Name the metadata file of a source asset ([`metadata_path_for`]): the
//!   source's normalized path with `.meta` appended.
//! - Define what an asset type needs to be loaded through its metadata
//!   ([`ImportedAsset`]): import settings and a decoder that takes them.
//! - Load an asset through its metadata file ([`AssetManager::import`]): read
//!   the guid and settings when the file exists, decode the source, and write
//!   the file when it is missing and the policy asks for it.
//! - Decode a loaded asset again in place ([`AssetManager::reimport`]), after
//!   its source or its metadata file changed.
//! - Follow a source moved with its metadata file
//!   ([`AssetManager::follow_move`]): the loaded asset takes the new name and
//!   keeps its handle and guid.
//!
//! # Design
//!
//! A source file in a project's `res` directory (`textures/helmet.jpg`) can
//! carry a metadata file beside it (`textures/helmet.jpg.meta`) holding what
//! the source does not say for itself - the asset's stable guid and its import
//! settings, never the decoded payload. The extension is kept rather than
//! replaced, so `helmet.png` and `helmet.obj` never share one. A `.meta` file
//! is only ever such a sidecar; it is never an asset of its own.
//!
//! The file is JSON with a small header the engine owns and the settings the
//! asset type owns:
//!
//! ```json
//! {
//!   "format_version": 1,
//!   "asset_type": "pill_master_renderer::assets::Texture",
//!   "guid": "9c0e4b7a1f2d3e4c5b6a798812345678",
//!   "settings": { "texture_type": "Normal" }
//! }
//! ```
//!
//! An asset loaded this way is named by its normalized path relative to `res`
//! and keyed by the guid in its metadata. One path is one asset: importing a
//! path that is already loaded returns the handle it already has, which is
//! exactly what a project reload re-running its loading code needs. A guid
//! claimed by a second path (a source copied together with its `.meta`) is
//! refused rather than silently moved, because the store would otherwise
//! rebind it.
//!
//! Nothing here stores a function pointer: [`AssetManager::import`] is generic,
//! so each call is compiled into the binary that makes it, and a reload has
//! nothing new to re-point. Code that does not know an asset's type goes
//! through [`ImportRegistry`](crate::asset_import_registry::ImportRegistry),
//! which does store them and handles the reload hazard that brings.

// Standard library
use std::path::{Path, PathBuf};

// External crates
use serde::de::DeserializeOwned;
use serde::Serialize;
use trait_type_map::TraitAccessible;

// Current crate
use crate::asset::{Asset, AssetGuid, AssetLoadError, AssetLoadResult, AssetManager, Handle};
use crate::asset_store::{self, pack_key};

/// The extension every metadata file carries, appended to its source's name.
pub const METADATA_EXTENSION: &str = "meta";

/// The metadata header layout this engine writes, and the newest it reads.
pub const METADATA_FORMAT_VERSION: u32 = 1;

/// The metadata file for the source asset at `source`, relative to the
/// project's asset directory, or `None` when `source` cannot have one.
///
/// The path is normalized the way an asset name is (`/`-separated, `.`
/// removed, by the same rules as the asset store's pack key) and `.meta` is
/// appended to the full file name: `textures\helmet.jpg` gives
/// `textures/helmet.jpg.meta`. `None` for a path that does not normalize (absolute, or climbing out with `..`), and for one
/// that is already a metadata file.
pub fn metadata_path_for(source: &Path) -> Option<PathBuf> {
    let name = pack_key(source)?;
    if is_metadata_file(&name) {
        return None;
    }
    Some(PathBuf::from(format!("{name}.{METADATA_EXTENSION}")))
}

/// Whether `name` is itself a metadata file.
fn is_metadata_file(name: &str) -> bool {
    Path::new(name)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case(METADATA_EXTENSION))
}

// =============================================================================
// ImportedAsset
// =============================================================================

/// An asset built from a source file in `res`, with import settings that can
/// live in an `<asset_name>.meta` file beside it.
///
/// Implemented by the crate that owns the asset type, next to its other
/// constructors; [`AssetManager::import`] is the one caller.
pub trait ImportedAsset: Asset + TraitAccessible<dyn Asset> + Sized {
    /// Everything the source bytes do not say for themselves.
    ///
    /// Every field becomes an on-disk contract, so keep the list short, mark
    /// the struct `#[serde(default)]` so a file written before a field existed
    /// still reads, and use `BTreeMap` rather than `HashMap` for any map: a
    /// value stored in the world outlives the binary that built it, and an
    /// empty `HashMap` points into that binary.
    type ImportSettings: Serialize + DeserializeOwned + Default + Clone;

    /// The source file extensions this type is imported from, lowercase and
    /// without the dot (`["png", "jpg"]`).
    ///
    /// The [`ImportRegistry`](crate::asset_import_registry::ImportRegistry)
    /// maps a file to its asset type by these, so the mapping comes from the
    /// types themselves rather than from a list in the engine. Two types may
    /// not claim one extension.
    const SOURCE_EXTENSIONS: &'static [&'static str];

    /// The stable name written to a metadata file's `asset_type`, which a
    /// file must match to be read as this type.
    ///
    /// Defaults to [`Asset::shared_name`]. A type without a shared name falls
    /// back to its Rust type name, which changes when the type moves, so such
    /// a type should override this with a pinned literal.
    fn metadata_type_name() -> &'static str {
        Self::shared_name().unwrap_or_else(std::any::type_name::<Self>)
    }

    /// Decode `source_bytes` into an asset with the given settings.
    ///
    /// `name` is the asset's normalized path, for the asset's own label and
    /// for error messages.
    ///
    /// # Errors
    ///
    /// Returns an [`AssetLoadError`] (usually `Decode`) when the bytes cannot
    /// be read as this asset with these settings.
    fn import(
        name: &str,
        source_bytes: &[u8],
        settings: &Self::ImportSettings,
    ) -> AssetLoadResult<Self>;
}

// =============================================================================
// Requests and outcomes
// =============================================================================

/// What [`AssetManager::import`] does when an asset has no metadata file yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataPolicy {
    /// Build the asset from the request's initial settings and write nothing.
    ReadIfPresent,
    /// The same, then write the metadata file beside the source - when the
    /// source is on a writable filesystem. A packed source (every shipping or
    /// web build) behaves as [`Self::ReadIfPresent`].
    CreateIfMissing,
}

/// One request to [`AssetManager::import`].
pub struct AssetImport<T: ImportedAsset> {
    /// The source file, relative to the project's asset directory.
    pub path: PathBuf,
    /// What to do when the source has no metadata file.
    pub policy: MetadataPolicy,
    /// The settings used when no metadata file exists. An existing file's
    /// settings always win over these.
    pub initial_settings: T::ImportSettings,
}

impl<T: ImportedAsset> AssetImport<T> {
    /// A request for `path` with the type's default initial settings.
    pub fn new(path: impl Into<PathBuf>, policy: MetadataPolicy) -> Self {
        Self {
            path: path.into(),
            policy,
            initial_settings: T::ImportSettings::default(),
        }
    }

    /// Use `settings` when the source has no metadata file yet.
    pub fn with_initial_settings(mut self, settings: T::ImportSettings) -> Self {
        self.initial_settings = settings;
        self
    }
}

/// Where an imported asset's guid and settings came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataSource {
    /// An existing metadata file, packed or on disk.
    ReadFromFile,
    /// No file existed; this import wrote one.
    CreatedOnDisk,
    /// No file existed and none was written: the policy did not ask for one,
    /// or the source is not on a writable filesystem. The guid lasts only for
    /// this run.
    InMemoryOnly,
}

/// The result of a successful [`AssetManager::import`].
pub struct ImportOutcome<T: Asset> {
    /// The asset's handle.
    pub handle: Handle<T>,
    /// The asset's guid.
    pub guid: AssetGuid,
    /// Where the guid and settings came from.
    pub metadata: MetadataSource,
    /// `true` when the path was already loaded and its existing handle was
    /// returned; nothing was read, decoded or written.
    pub already_loaded: bool,
}

// Written out by hand: a derive would require `T: Debug`, and a handle stores
// no `T`.
impl<T: Asset> std::fmt::Debug for ImportOutcome<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ImportOutcome")
            .field("handle", &self.handle)
            .field("guid", &self.guid)
            .field("metadata", &self.metadata)
            .field("already_loaded", &self.already_loaded)
            .finish()
    }
}

/// The result of a successful [`AssetManager::reimport`].
pub struct ReimportOutcome<T: Asset> {
    /// The asset's handle: the one it already had when it was loaded.
    pub handle: Handle<T>,
    /// The asset's guid.
    pub guid: AssetGuid,
    /// Where the settings came from: [`MetadataSource::ReadFromFile`], or
    /// [`MetadataSource::InMemoryOnly`] when the source has no metadata file
    /// and the type's default settings were used.
    pub metadata: MetadataSource,
    /// `true` when the loaded value was replaced in its slot; `false` when the
    /// path was not loaded and was imported fresh instead.
    pub replaced: bool,
}

// Written out by hand for the same reason as `ImportOutcome`'s.
impl<T: Asset> std::fmt::Debug for ReimportOutcome<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReimportOutcome")
            .field("handle", &self.handle)
            .field("guid", &self.guid)
            .field("metadata", &self.metadata)
            .field("replaced", &self.replaced)
            .finish()
    }
}

/// Why [`AssetManager::import`] did not load an asset.
#[derive(Debug, thiserror::Error)]
pub enum AssetImportError {
    /// Reading or decoding the source or its metadata file failed.
    #[error(transparent)]
    Load(#[from] AssetLoadError),
    /// The path is already loaded, but not under the guid its metadata file
    /// now holds - the file was edited, or the name was taken by an asset
    /// added without metadata (`loaded` is then `None`).
    #[error("asset `{path}` is loaded under guid {}, but its metadata says {metadata}", display_guid(.loaded))]
    PathBoundToOtherGuid {
        /// The asset's normalized path.
        path: String,
        /// The guid the loaded asset has, if any.
        loaded: Option<AssetGuid>,
        /// The guid in the metadata file.
        metadata: AssetGuid,
    },
    /// The metadata file's guid already belongs to another live asset of this
    /// type - usually a source copied together with its `.meta`.
    #[error(
        "asset `{path}` claims guid {guid}, which `{other_path}` already has; \
         was it copied together with its .meta file?"
    )]
    DuplicateGuid {
        /// The contested guid.
        guid: AssetGuid,
        /// The path being imported.
        path: String,
        /// The path that already owns the guid.
        other_path: String,
    },
}

/// A guid for an error message, or "none".
fn display_guid(guid: &Option<AssetGuid>) -> String {
    guid.map_or_else(|| "none".to_owned(), |guid| guid.to_string())
}

// =============================================================================
// The metadata file
// =============================================================================

/// The metadata file as written: the header, then the type's settings.
#[derive(Serialize)]
struct MetadataDocument<'settings, S> {
    format_version: u32,
    asset_type: &'static str,
    guid: AssetGuid,
    settings: &'settings S,
}

/// What a metadata file says, once checked against the asset type.
struct StoredMetadata<S> {
    guid: AssetGuid,
    settings: S,
}

/// Parse a metadata file for `T`.
///
/// The header is checked before the settings are read, so the error says what
/// is actually wrong: a newer format, another asset type, or settings that do
/// not fit this type. A missing `settings` object means "all defaults".
fn parse_metadata<T: ImportedAsset>(
    path: &Path,
    bytes: &[u8],
) -> AssetLoadResult<StoredMetadata<T::ImportSettings>> {
    let (guid, settings) = parse_header(path, bytes, T::metadata_type_name(), "settings")?;
    let settings = match settings {
        None => T::ImportSettings::default(),
        Some(settings) => {
            serde_json::from_value(settings).map_err(|error| AssetLoadError::Metadata {
                path: path.to_owned(),
                detail: format!("`settings` do not fit this asset type: {error}"),
            })?
        }
    };
    Ok(StoredMetadata { guid, settings })
}

/// Check the header every asset file shares - a `.meta` sidecar or a
/// standalone asset - and return its guid and the value under `body_field`
/// (`None` when absent or `null`).
///
/// The header is checked before the body is looked at, so the error says what
/// is actually wrong: a newer format, another asset type, or a bad guid.
pub(crate) fn parse_header(
    path: &Path,
    bytes: &[u8],
    expected_type: &str,
    body_field: &str,
) -> AssetLoadResult<(AssetGuid, Option<serde_json::Value>)> {
    let fail = |detail: String| AssetLoadError::Metadata {
        path: path.to_owned(),
        detail,
    };
    let document: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|error| fail(format!("not valid JSON: {error}")))?;
    let header = document
        .as_object()
        .ok_or_else(|| fail("not a JSON object".to_owned()))?;

    let version = header
        .get("format_version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| fail("`format_version` is missing".to_owned()))?;
    if version == 0 || version > u64::from(METADATA_FORMAT_VERSION) {
        return Err(fail(format!(
            "format version {version} is not supported; this engine reads up to {METADATA_FORMAT_VERSION}"
        )));
    }

    let asset_type = header
        .get("asset_type")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| fail("`asset_type` is missing".to_owned()))?;
    if asset_type != expected_type {
        return Err(fail(format!(
            "it describes a `{asset_type}`, not a `{expected_type}`"
        )));
    }

    let guid = header
        .get("guid")
        .and_then(serde_json::Value::as_str)
        .and_then(AssetGuid::parse)
        .ok_or_else(|| fail("`guid` is missing or not 32 hexadecimal digits".to_owned()))?;

    let body = match header.get(body_field) {
        None | Some(serde_json::Value::Null) => None,
        Some(body) => Some(body.clone()),
    };
    Ok((guid, body))
}

/// Whether two settings values are the same, compared through their JSON form
/// so that `ImportSettings` need not implement `PartialEq`.
fn same_settings<T: ImportedAsset>(left: &T::ImportSettings, right: &T::ImportSettings) -> bool {
    match (serde_json::to_value(left), serde_json::to_value(right)) {
        (Ok(left), Ok(right)) => left == right,
        // Settings that cannot be compared are treated as different, which
        // costs at most one extra decode.
        _ => false,
    }
}

/// The bytes of a metadata file for `T` holding `guid` and `settings`.
fn render_metadata<T: ImportedAsset>(
    path: &Path,
    guid: AssetGuid,
    settings: &T::ImportSettings,
) -> AssetLoadResult<Vec<u8>> {
    let document = MetadataDocument {
        format_version: METADATA_FORMAT_VERSION,
        asset_type: T::metadata_type_name(),
        guid,
        settings,
    };
    let mut bytes =
        serde_json::to_vec_pretty(&document).map_err(|error| AssetLoadError::Metadata {
            path: path.to_owned(),
            detail: format!("the settings cannot be written as JSON: {error}"),
        })?;
    bytes.push(b'\n');
    Ok(bytes)
}

// =============================================================================
// AssetManager::import
// =============================================================================

impl AssetManager {
    /// Load the source asset at `request.path` through its metadata file.
    ///
    /// The asset is named by its normalized path and keyed by the guid in its
    /// metadata. When the metadata file exists (packed or on disk), its guid
    /// and settings are used; when it does not, the request's initial
    /// settings and a new random guid are, and with
    /// [`MetadataPolicy::CreateIfMissing`] the file is written beside the
    /// source - after the source decoded, so a broken source never leaves a
    /// metadata file behind. A file that exists is never overwritten, even
    /// when it cannot be read.
    ///
    /// Importing a path that is already loaded returns its existing handle
    /// with [`ImportOutcome::already_loaded`] set, and reads nothing.
    ///
    /// # Errors
    ///
    /// - [`AssetImportError::Load`] when the path does not normalize or is a
    ///   metadata file, the source cannot be read or decoded, or the metadata
    ///   file is unreadable, for another asset type, or from a newer format.
    /// - [`AssetImportError::PathBoundToOtherGuid`] when the path is loaded
    ///   under a different guid than its metadata file now holds.
    /// - [`AssetImportError::DuplicateGuid`] when another live asset of this
    ///   type already has the metadata file's guid.
    pub fn import<T: ImportedAsset>(
        &mut self,
        request: AssetImport<T>,
    ) -> Result<ImportOutcome<T>, AssetImportError> {
        let AssetImport {
            path,
            policy,
            initial_settings,
        } = request;

        // Step 1: the asset's name and its metadata file's path.
        let asset_name =
            pack_key(&path).ok_or_else(|| AssetLoadError::PathNotFound { path: path.clone() })?;
        let metadata_path =
            metadata_path_for(Path::new(&asset_name)).ok_or_else(|| AssetLoadError::Metadata {
                path: path.clone(),
                detail: "a .meta file describes an asset; it is not one".to_owned(),
            })?;

        // Step 2: what the metadata file says, when there is one.
        let stored = match asset_store::read(&metadata_path) {
            Ok(bytes) => Some(parse_metadata::<T>(&metadata_path, &bytes)?),
            Err(AssetLoadError::PathNotFound { .. }) => None,
            Err(error) => return Err(error.into()),
        };

        // Step 3: a path that is already loaded is either this same asset or a
        // conflict. Without a metadata file there is no guid to disagree with,
        // so the loaded asset's own guid stands.
        if let Some(handle) = self.handle_by_name::<T>(&asset_name) {
            let loaded = self.guid_of(handle);
            return match (&stored, loaded) {
                (Some(stored), Some(loaded)) if stored.guid == loaded => Ok(ImportOutcome {
                    handle,
                    guid: loaded,
                    metadata: MetadataSource::ReadFromFile,
                    already_loaded: true,
                }),
                (None, Some(loaded)) => Ok(ImportOutcome {
                    handle,
                    guid: loaded,
                    metadata: MetadataSource::InMemoryOnly,
                    already_loaded: true,
                }),
                (Some(stored), loaded) => Err(AssetImportError::PathBoundToOtherGuid {
                    path: asset_name,
                    loaded,
                    metadata: stored.guid,
                }),
                (None, None) => Err(AssetImportError::PathBoundToOtherGuid {
                    path: asset_name,
                    loaded: None,
                    // There is no metadata guid either; report the conflict
                    // against the nil guid rather than inventing one.
                    metadata: AssetGuid::new(0),
                }),
            };
        }

        // The guid and settings this import uses. A new guid is drawn only
        // here, once the asset is known to be new.
        let has_metadata_file = stored.is_some();
        let (guid, settings) = match stored {
            Some(stored) => (stored.guid, stored.settings),
            None => {
                let guid = AssetGuid::random().map_err(|error| AssetLoadError::Metadata {
                    path: metadata_path.clone(),
                    detail: format!("no random source for a new guid: {error}"),
                })?;
                (guid, initial_settings)
            }
        };

        // Step 4: the store would silently rebind a guid another asset holds.
        if let Some(other) = self.handle_by_guid::<T>(guid) {
            return Err(AssetImportError::DuplicateGuid {
                guid,
                path: asset_name,
                other_path: self.name_of(other).unwrap_or("<unnamed>").to_owned(),
            });
        }

        // Step 5: decode the source.
        let source_bytes = asset_store::read(Path::new(&asset_name))?;
        let mut asset = T::import(&asset_name, &source_bytes, &settings)?;
        let mut guid = guid;

        // Step 6: record the guid and settings when asked to and possible. A
        // write failure costs only the persistence of this run's guid, so it is
        // reported but does not refuse an asset that decoded.
        let mut metadata = if has_metadata_file {
            MetadataSource::ReadFromFile
        } else {
            MetadataSource::InMemoryOnly
        };
        if !has_metadata_file && policy == MetadataPolicy::CreateIfMissing {
            let document = render_metadata::<T>(&metadata_path, guid, &settings)?;
            match asset_store::write_beside(Path::new(&asset_name), &metadata_path, &document) {
                Ok(Some(_)) => {
                    // Another writer - a second host, a parallel test - may
                    // have created the file between step 2 and now, and
                    // `write_beside` keeps theirs. The file is the truth, so
                    // read it back and adopt it if it is not ours.
                    let written = asset_store::read(&metadata_path)?;
                    let on_disk = parse_metadata::<T>(&metadata_path, &written)?;
                    if on_disk.guid == guid {
                        metadata = MetadataSource::CreatedOnDisk;
                    } else {
                        if let Some(other) = self.handle_by_guid::<T>(on_disk.guid) {
                            return Err(AssetImportError::DuplicateGuid {
                                guid: on_disk.guid,
                                path: asset_name,
                                other_path: self.name_of(other).unwrap_or("<unnamed>").to_owned(),
                            });
                        }
                        if !same_settings::<T>(&on_disk.settings, &settings) {
                            asset = T::import(&asset_name, &source_bytes, &on_disk.settings)?;
                        }
                        guid = on_disk.guid;
                        metadata = MetadataSource::ReadFromFile;
                    }
                }
                Ok(None) => pill_core::debug!(
                    "asset `{asset_name}` is not on a writable filesystem; its metadata was not written"
                ),
                Err(error) => pill_core::warn!(
                    "asset `{asset_name}` loaded, but its metadata could not be written: {error}"
                ),
            }
        }

        // Step 7: steps 3 and 4 ruled out both conflicts.
        let handle = self
            .add_named_with_guid(asset_name, guid, asset)
            .expect("the path was checked to be unloaded before decoding");
        Ok(ImportOutcome {
            handle,
            guid,
            metadata,
            already_loaded: false,
        })
    }
}

// =============================================================================
// AssetManager::reimport
// =============================================================================

impl AssetManager {
    /// Decode the asset at `path` again and replace the loaded value in its
    /// slot, after its source or its metadata file changed.
    ///
    /// The handle, name and guid stay what they were, and the slot's content
    /// version moves, so whatever holds the handle sees the new value and a
    /// renderer re-uploads only this asset. The settings come from the
    /// metadata file, or are the type's defaults when there is none. Nothing
    /// is written.
    ///
    /// A path that is not loaded is imported instead, with
    /// [`MetadataPolicy::ReadIfPresent`]; the outcome's `replaced` is then
    /// `false`.
    ///
    /// # Errors
    ///
    /// The errors of [`Self::import`]. In particular a metadata file whose
    /// guid no longer matches the loaded asset is
    /// [`AssetImportError::PathBoundToOtherGuid`]. On every error the loaded
    /// value is left exactly as it was: a source that fails to decode never
    /// empties its slot.
    pub fn reimport<T: ImportedAsset>(
        &mut self,
        path: &Path,
    ) -> Result<ReimportOutcome<T>, AssetImportError> {
        let asset_name = pack_key(path).ok_or_else(|| AssetLoadError::PathNotFound {
            path: path.to_owned(),
        })?;
        let Some(handle) = self.handle_by_name::<T>(&asset_name) else {
            let imported =
                self.import(AssetImport::<T>::new(path, MetadataPolicy::ReadIfPresent))?;
            return Ok(ReimportOutcome {
                handle: imported.handle,
                guid: imported.guid,
                metadata: imported.metadata,
                replaced: false,
            });
        };
        let metadata_path =
            metadata_path_for(Path::new(&asset_name)).ok_or_else(|| AssetLoadError::Metadata {
                path: path.to_owned(),
                detail: "a .meta file describes an asset; it is not one".to_owned(),
            })?;

        // The metadata file decides the settings; a guid it changed under a
        // loaded asset is refused rather than followed, because handles and
        // references elsewhere still name the old one.
        let loaded = self.guid_of(handle);
        let (settings, metadata) = match asset_store::read(&metadata_path) {
            Ok(bytes) => {
                let stored = parse_metadata::<T>(&metadata_path, &bytes)?;
                if Some(stored.guid) != loaded {
                    return Err(AssetImportError::PathBoundToOtherGuid {
                        path: asset_name,
                        loaded,
                        metadata: stored.guid,
                    });
                }
                (stored.settings, MetadataSource::ReadFromFile)
            }
            Err(AssetLoadError::PathNotFound { .. }) => {
                (T::ImportSettings::default(), MetadataSource::InMemoryOnly)
            }
            Err(error) => return Err(error.into()),
        };

        // Decode fully before touching the slot, so a failure leaves it as is.
        let source_bytes = asset_store::read(Path::new(&asset_name))?;
        let asset = T::import(&asset_name, &source_bytes, &settings)?;
        *self
            .get_mut(handle)
            .expect("the handle was looked up by name just above") = asset;
        Ok(ReimportOutcome {
            handle,
            guid: loaded.unwrap_or(AssetGuid::new(0)),
            metadata,
            replaced: true,
        })
    }
}

// =============================================================================
// AssetManager::follow_move
// =============================================================================

impl AssetManager {
    /// When the source at `path` is a loaded asset that was moved here
    /// together with its metadata file, rename that asset to `path` and
    /// return its old name.
    ///
    /// A move is recognized by the guid: `path`'s metadata file holds the guid
    /// of a loaded asset that has another name, and that asset's source is no
    /// longer where its name says. The asset keeps its handle, guid and value -
    /// nothing is decoded - so whatever holds it is unaffected.
    ///
    /// Returns `Ok(None)` - and changes nothing - when `path` is already
    /// loaded, has no metadata file, holds a guid no loaded asset has, or when
    /// the other asset's source still exists: that is a copy, which importing
    /// reports as [`AssetImportError::DuplicateGuid`].
    ///
    /// # Errors
    ///
    /// When the path does not normalize, or its metadata file exists but does
    /// not read as `T`'s.
    pub fn follow_move<T: ImportedAsset>(
        &mut self,
        path: &Path,
    ) -> Result<Option<String>, AssetImportError> {
        let asset_name = pack_key(path).ok_or_else(|| AssetLoadError::PathNotFound {
            path: path.to_owned(),
        })?;
        if self.handle_by_name::<T>(&asset_name).is_some() {
            return Ok(None);
        }
        let Some(metadata_path) = metadata_path_for(Path::new(&asset_name)) else {
            return Ok(None);
        };
        let stored = match asset_store::read(&metadata_path) {
            Ok(bytes) => parse_metadata::<T>(&metadata_path, &bytes)?,
            Err(AssetLoadError::PathNotFound { .. }) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let Some(handle) = self.handle_by_guid::<T>(stored.guid) else {
            return Ok(None);
        };
        let Some(old_name) = self.name_of(handle).map(str::to_owned) else {
            return Ok(None);
        };
        // The old source still being there means two files claim one guid.
        if asset_store::locate_file(Path::new(&old_name)).is_some() {
            return Ok(None);
        }
        self.rename(handle, asset_name.clone())
            .map_err(|error| AssetLoadError::Metadata {
                path: metadata_path.clone(),
                detail: format!("the moved asset could not take its new name: {error}"),
            })?;
        Ok(Some(old_name))
    }
}

// =============================================================================
// Settings as JSON
// =============================================================================

/// The import settings `T` would use for the source at `path`, as JSON: the
/// metadata file's, or the type's defaults when there is none.
///
/// For code that edits settings without knowing `T` (an editor's inspector),
/// through the import registry.
///
/// # Errors
///
/// Returns an [`AssetLoadError`] when the path does not normalize, or its
/// metadata file exists but cannot be read as `T`'s.
pub fn settings_json_for<T: ImportedAsset>(path: &Path) -> AssetLoadResult<serde_json::Value> {
    let metadata_path = metadata_path_for(path).ok_or_else(|| AssetLoadError::PathNotFound {
        path: path.to_owned(),
    })?;
    let settings = match asset_store::read(&metadata_path) {
        Ok(bytes) => parse_metadata::<T>(&metadata_path, &bytes)?.settings,
        Err(AssetLoadError::PathNotFound { .. }) => T::ImportSettings::default(),
        Err(error) => return Err(error),
    };
    settings_to_json::<T>(path, &settings)
}

/// `value` read as `T`'s import settings and written back, which fills every
/// missing field with its default and drops unknown ones.
///
/// # Errors
///
/// Returns [`AssetLoadError::Metadata`] when `value` does not fit `T`'s
/// settings.
pub fn normalize_settings_json<T: ImportedAsset>(
    value: serde_json::Value,
) -> AssetLoadResult<serde_json::Value> {
    let label = Path::new(T::metadata_type_name());
    let settings: T::ImportSettings =
        serde_json::from_value(value).map_err(|error| AssetLoadError::Metadata {
            path: label.to_owned(),
            detail: format!("the settings do not fit this asset type: {error}"),
        })?;
    settings_to_json::<T>(label, &settings)
}

/// `settings` as a JSON value, with `path` naming the failure.
fn settings_to_json<T: ImportedAsset>(
    path: &Path,
    settings: &T::ImportSettings,
) -> AssetLoadResult<serde_json::Value> {
    serde_json::to_value(settings).map_err(|error| AssetLoadError::Metadata {
        path: path.to_owned(),
        detail: format!("the settings cannot be written as JSON: {error}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset_store::{mount_directory, mount_pack, mounted_directory_test_lock, AssetPack};
    use std::cell::Cell;
    use trait_type_map::impl_trait_accessible;

    // -------------------------------------------------------------------------
    // Path naming
    // -------------------------------------------------------------------------

    #[test]
    fn the_metadata_file_keeps_the_source_extension() {
        assert_eq!(
            metadata_path_for(Path::new("a.jpg")),
            Some(PathBuf::from("a.jpg.meta"))
        );
        // Two sources sharing a stem get two metadata files.
        assert_ne!(
            metadata_path_for(Path::new("helmet.png")),
            metadata_path_for(Path::new("helmet.obj"))
        );
    }

    #[test]
    fn nested_and_unnormalized_paths_name_one_file() {
        let expected = Some(PathBuf::from("textures/stone/a.jpg.meta"));
        assert_eq!(
            metadata_path_for(Path::new("textures/stone/a.jpg")),
            expected
        );
        assert_eq!(
            metadata_path_for(Path::new("textures\\stone\\a.jpg")),
            expected
        );
        assert_eq!(
            metadata_path_for(Path::new("./textures/./stone/a.jpg")),
            expected
        );
    }

    #[test]
    fn a_source_without_an_extension_has_metadata_too() {
        assert_eq!(
            metadata_path_for(Path::new("data/LICENSE")),
            Some(PathBuf::from("data/LICENSE.meta"))
        );
    }

    #[test]
    fn metadata_files_and_paths_outside_res_have_none() {
        assert_eq!(metadata_path_for(Path::new("a.jpg.meta")), None);
        assert_eq!(metadata_path_for(Path::new("a.jpg.META")), None);
        assert_eq!(metadata_path_for(Path::new("../a.jpg")), None);
        assert_eq!(metadata_path_for(Path::new("")), None);
        let absolute = std::env::temp_dir().join("a.jpg");
        assert_eq!(metadata_path_for(&absolute), None);
    }

    // -------------------------------------------------------------------------
    // A dummy imported asset
    // -------------------------------------------------------------------------

    /// Two settings, with defaults that differ from any test's initial ones.
    #[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
    #[serde(default)]
    struct NoteSettings {
        volume: u32,
        label: String,
    }

    impl Default for NoteSettings {
        fn default() -> Self {
            Self {
                volume: 7,
                label: "default".to_owned(),
            }
        }
    }

    /// The decoded asset: the source text plus the settings it was read with.
    #[derive(Debug, PartialEq)]
    struct Note {
        name: String,
        text: String,
        settings: NoteSettings,
    }

    impl Asset for Note {}
    impl_trait_accessible!(dyn Asset; Note);

    thread_local! {
        /// How many times `Note::import` ran on this test's thread.
        static DECODES: Cell<usize> = const { Cell::new(0) };
    }

    impl ImportedAsset for Note {
        type ImportSettings = NoteSettings;
        const SOURCE_EXTENSIONS: &'static [&'static str] = &["note"];

        fn metadata_type_name() -> &'static str {
            "pill_engine::asset_metadata::tests::Note"
        }

        fn import(
            name: &str,
            source_bytes: &[u8],
            settings: &NoteSettings,
        ) -> AssetLoadResult<Self> {
            DECODES.with(|decodes| decodes.set(decodes.get() + 1));
            let text =
                String::from_utf8(source_bytes.to_vec()).map_err(|_| AssetLoadError::Decode {
                    label: name.to_owned(),
                    detail: "not UTF-8".to_owned(),
                })?;
            if text == "broken" {
                return Err(AssetLoadError::Decode {
                    label: name.to_owned(),
                    detail: "the source says it is broken".to_owned(),
                });
            }
            Ok(Self {
                name: name.to_owned(),
                text,
                settings: settings.clone(),
            })
        }
    }

    fn decodes() -> usize {
        DECODES.with(Cell::get)
    }

    /// A scratch `res` directory, mounted for the test's duration under the
    /// mounted-directory lock, and removed afterwards.
    struct ScratchRes {
        root: PathBuf,
        _mounted: std::sync::MutexGuard<'static, ()>,
    }

    impl ScratchRes {
        fn new(label: &str) -> Self {
            let mounted = mounted_directory_test_lock();
            static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let sequence = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "pill-asset-metadata-{label}-{}-{sequence}",
                std::process::id()
            ));
            std::fs::create_dir_all(&root).unwrap();
            mount_directory(&root);
            Self {
                root,
                _mounted: mounted,
            }
        }

        fn write(&self, relative: &str, contents: &str) {
            let path = self.root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }

        fn read(&self, relative: &str) -> Option<String> {
            std::fs::read_to_string(self.root.join(relative)).ok()
        }
    }

    impl Drop for ScratchRes {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn settings(volume: u32, label: &str) -> NoteSettings {
        NoteSettings {
            volume,
            label: label.to_owned(),
        }
    }

    fn request(path: &str, policy: MetadataPolicy) -> AssetImport<Note> {
        AssetImport::new(path, policy).with_initial_settings(settings(1, "initial"))
    }

    fn metadata_json(guid: &str, settings: &str) -> String {
        format!(
            r#"{{"format_version": 1, "asset_type": "pill_engine::asset_metadata::tests::Note", "guid": "{guid}", "settings": {settings}}}"#
        )
    }

    const GUID_A: &str = "0000000000000000000000000000000a";
    const GUID_B: &str = "0000000000000000000000000000000b";

    // -------------------------------------------------------------------------
    // Import
    // -------------------------------------------------------------------------

    #[test]
    fn missing_metadata_read_if_present_builds_and_writes_nothing() {
        let res = ScratchRes::new("read-if-present");
        res.write("notes/a.note", "hello");
        let mut assets = AssetManager::new();

        let outcome = assets
            .import(request("notes/a.note", MetadataPolicy::ReadIfPresent))
            .unwrap();

        assert_eq!(outcome.metadata, MetadataSource::InMemoryOnly);
        assert!(!outcome.already_loaded);
        let note = assets.get(outcome.handle).unwrap();
        assert_eq!(note.text, "hello");
        assert_eq!(note.settings, settings(1, "initial"));
        assert_eq!(res.read("notes/a.note.meta"), None);
    }

    #[test]
    fn missing_metadata_create_if_missing_writes_what_was_used() {
        let res = ScratchRes::new("create");
        res.write("notes/a.note", "hello");
        let mut assets = AssetManager::new();

        let outcome = assets
            .import(request("notes/a.note", MetadataPolicy::CreateIfMissing))
            .unwrap();

        assert_eq!(outcome.metadata, MetadataSource::CreatedOnDisk);
        let written = res
            .read("notes/a.note.meta")
            .expect("the metadata file was written");
        let stored =
            parse_metadata::<Note>(Path::new("notes/a.note.meta"), written.as_bytes()).unwrap();
        assert_eq!(stored.guid, outcome.guid);
        assert_eq!(stored.settings, settings(1, "initial"));
    }

    #[test]
    fn an_existing_metadata_file_beats_the_initial_settings() {
        let res = ScratchRes::new("existing");
        res.write("a.note", "hello");
        res.write(
            "a.note.meta",
            &metadata_json(GUID_A, r#"{"volume": 3, "label": "file"}"#),
        );
        let mut assets = AssetManager::new();

        let outcome = assets
            .import(request("a.note", MetadataPolicy::CreateIfMissing))
            .unwrap();

        assert_eq!(outcome.metadata, MetadataSource::ReadFromFile);
        assert_eq!(outcome.guid, AssetGuid::parse(GUID_A).unwrap());
        assert_eq!(
            assets.get(outcome.handle).unwrap().settings,
            settings(3, "file")
        );
    }

    #[test]
    fn unknown_fields_are_ignored_and_missing_fields_default() {
        let res = ScratchRes::new("tolerant");
        res.write("a.note", "hello");
        res.write(
            "a.note.meta",
            &metadata_json(GUID_A, r#"{"label": "file", "added_later": true}"#),
        );
        let mut assets = AssetManager::new();

        let outcome = assets
            .import(request("a.note", MetadataPolicy::ReadIfPresent))
            .unwrap();

        // `volume` was missing: the type's default, not the initial settings.
        assert_eq!(
            assets.get(outcome.handle).unwrap().settings,
            settings(7, "file")
        );
    }

    /// A file that cannot be used is reported and left exactly as it was.
    #[test]
    fn unusable_metadata_is_an_error_and_is_never_overwritten() {
        let cases = [
            (
                "wrong-type",
                r#"{"format_version": 1, "asset_type": "Texture", "guid": "0000000000000000000000000000000a"}"#,
            ),
            (
                "newer",
                r#"{"format_version": 99, "asset_type": "pill_engine::asset_metadata::tests::Note", "guid": "0000000000000000000000000000000a"}"#,
            ),
            ("broken-json", r#"{"format_version": 1, "#),
        ];
        for (label, contents) in cases {
            let res = ScratchRes::new(label);
            res.write("a.note", "hello");
            res.write("a.note.meta", contents);
            let mut assets = AssetManager::new();

            let result = assets.import(request("a.note", MetadataPolicy::CreateIfMissing));

            assert!(
                matches!(
                    result,
                    Err(AssetImportError::Load(AssetLoadError::Metadata { .. }))
                ),
                "{label}: {result:?}"
            );
            assert_eq!(
                res.read("a.note.meta").as_deref(),
                Some(contents),
                "{label}"
            );
            assert!(assets.is_empty::<Note>(), "{label}");
        }
    }

    #[test]
    fn a_source_that_fails_to_decode_leaves_no_metadata_file() {
        let res = ScratchRes::new("broken-source");
        res.write("a.note", "broken");
        let mut assets = AssetManager::new();

        let result = assets.import(request("a.note", MetadataPolicy::CreateIfMissing));

        assert!(matches!(
            result,
            Err(AssetImportError::Load(AssetLoadError::Decode { .. }))
        ));
        assert_eq!(res.read("a.note.meta"), None);
    }

    #[test]
    fn a_packed_source_is_loaded_but_nothing_is_written() {
        let res = ScratchRes::new("packed-source");
        let key = "pill-meta-test/packed-source/a.note";
        mount_pack(AssetPack::parse(pack_of(&[(key, b"from the pack")])).unwrap());
        let mut assets = AssetManager::new();

        let outcome = assets
            .import(request(key, MetadataPolicy::CreateIfMissing))
            .unwrap();

        assert_eq!(outcome.metadata, MetadataSource::InMemoryOnly);
        assert_eq!(assets.get(outcome.handle).unwrap().text, "from the pack");
        assert_eq!(
            asset_store::locate_file(Path::new(&format!("{key}.meta"))),
            None
        );
        assert_eq!(res.read(&format!("{key}.meta")), None);
    }

    /// A shipping build reads the metadata it was packed with, not a stray
    /// file at the same development path.
    #[test]
    fn packed_metadata_is_read_before_the_filesystem() {
        let res = ScratchRes::new("packed-metadata");
        let key = "pill-meta-test/packed-metadata/a.note";
        let packed = metadata_json(GUID_A, r#"{"volume": 2, "label": "packed"}"#);
        mount_pack(
            AssetPack::parse(pack_of(&[(&format!("{key}.meta"), packed.as_bytes())])).unwrap(),
        );
        res.write(key, "hello");
        res.write(
            &format!("{key}.meta"),
            &metadata_json(GUID_B, r#"{"volume": 9, "label": "on disk"}"#),
        );
        let mut assets = AssetManager::new();

        let outcome = assets
            .import(request(key, MetadataPolicy::ReadIfPresent))
            .unwrap();

        assert_eq!(outcome.guid, AssetGuid::parse(GUID_A).unwrap());
        assert_eq!(
            assets.get(outcome.handle).unwrap().settings,
            settings(2, "packed")
        );
    }

    #[test]
    fn the_asset_is_named_by_its_normalized_path_and_keyed_by_its_guid() {
        let res = ScratchRes::new("identity");
        res.write("notes/a.note", "hello");
        res.write("notes/a.note.meta", &metadata_json(GUID_A, "{}"));
        let mut assets = AssetManager::new();

        let outcome = assets
            .import(request("notes\\.\\a.note", MetadataPolicy::ReadIfPresent))
            .unwrap();

        assert_eq!(assets.name_of(outcome.handle), Some("notes/a.note"));
        assert_eq!(assets.get(outcome.handle).unwrap().name, "notes/a.note");
        let guid = AssetGuid::parse(GUID_A).unwrap();
        assert_eq!(assets.guid_of(outcome.handle), Some(guid));
        assert_eq!(assets.handle_by_guid::<Note>(guid), Some(outcome.handle));
    }

    /// What a project reload does: re-run its loading code against a store
    /// that already holds the assets.
    #[test]
    fn importing_a_loaded_path_returns_its_handle_without_decoding() {
        let res = ScratchRes::new("reimport");
        res.write("a.note", "hello");
        let mut assets = AssetManager::new();
        let first = assets
            .import(request("a.note", MetadataPolicy::CreateIfMissing))
            .unwrap();
        let decodes_after_first = decodes();

        let second = assets
            .import(request("a.note", MetadataPolicy::CreateIfMissing))
            .unwrap();

        assert!(second.already_loaded);
        assert_eq!(second.handle, first.handle);
        assert_eq!(second.guid, first.guid);
        assert_eq!(
            decodes(),
            decodes_after_first,
            "the source was decoded again"
        );
        assert_eq!(assets.len::<Note>(), 1);
    }

    /// The same, for a source with no metadata file: the loaded asset's guid
    /// stands instead of a new random one being drawn and disagreeing.
    #[test]
    fn importing_a_loaded_path_without_metadata_returns_its_handle() {
        let res = ScratchRes::new("reimport-no-metadata");
        res.write("a.note", "hello");
        let mut assets = AssetManager::new();
        let first = assets
            .import(request("a.note", MetadataPolicy::ReadIfPresent))
            .unwrap();

        let second = assets
            .import(request("a.note", MetadataPolicy::ReadIfPresent))
            .unwrap();

        assert!(second.already_loaded);
        assert_eq!((second.handle, second.guid), (first.handle, first.guid));
    }

    #[test]
    fn new_sources_get_different_guids() {
        let res = ScratchRes::new("distinct-guids");
        res.write("a.note", "a");
        res.write("b.note", "b");
        let mut assets = AssetManager::new();

        let a = assets
            .import(request("a.note", MetadataPolicy::ReadIfPresent))
            .unwrap();
        let b = assets
            .import(request("b.note", MetadataPolicy::ReadIfPresent))
            .unwrap();

        assert_ne!(a.guid, b.guid);
    }

    #[test]
    fn a_removed_asset_imports_again_under_the_same_guid() {
        let res = ScratchRes::new("remove");
        res.write("a.note", "hello");
        let mut assets = AssetManager::new();
        let first = assets
            .import(request("a.note", MetadataPolicy::CreateIfMissing))
            .unwrap();
        assets.remove(first.handle);

        let second = assets
            .import(request("a.note", MetadataPolicy::CreateIfMissing))
            .unwrap();

        assert!(!second.already_loaded);
        assert_ne!(second.handle, first.handle);
        assert_eq!(second.guid, first.guid);
        assert_eq!(second.metadata, MetadataSource::ReadFromFile);
    }

    #[test]
    fn a_copied_metadata_file_is_a_duplicate_guid() {
        let res = ScratchRes::new("duplicate");
        res.write("a.note", "a");
        res.write("a.note.meta", &metadata_json(GUID_A, "{}"));
        res.write("copy.note", "a");
        res.write("copy.note.meta", &metadata_json(GUID_A, "{}"));
        let mut assets = AssetManager::new();
        let original = assets
            .import(request("a.note", MetadataPolicy::ReadIfPresent))
            .unwrap();

        let result = assets.import(request("copy.note", MetadataPolicy::ReadIfPresent));

        assert!(matches!(
            result,
            Err(AssetImportError::DuplicateGuid { ref path, ref other_path, .. })
                if path == "copy.note" && other_path == "a.note"
        ));
        let guid = AssetGuid::parse(GUID_A).unwrap();
        assert_eq!(assets.handle_by_guid::<Note>(guid), Some(original.handle));
        assert_eq!(assets.len::<Note>(), 1);
    }

    #[test]
    fn a_metadata_guid_edited_while_loaded_is_a_conflict() {
        let res = ScratchRes::new("edited-guid");
        res.write("a.note", "hello");
        res.write("a.note.meta", &metadata_json(GUID_A, "{}"));
        let mut assets = AssetManager::new();
        assets
            .import(request("a.note", MetadataPolicy::ReadIfPresent))
            .unwrap();
        res.write("a.note.meta", &metadata_json(GUID_B, "{}"));

        let result = assets.import(request("a.note", MetadataPolicy::ReadIfPresent));

        assert!(matches!(
            result,
            Err(AssetImportError::PathBoundToOtherGuid { loaded: Some(loaded), metadata, .. })
                if loaded == AssetGuid::parse(GUID_A).unwrap() && metadata == AssetGuid::parse(GUID_B).unwrap()
        ));
    }

    /// Writers racing to create one metadata file - two hosts, parallel tests -
    /// all end up holding the guid of the file that won, never a private one
    /// that the next import would then report as a conflict.
    #[test]
    fn racing_first_imports_agree_on_the_written_guid() {
        let res = ScratchRes::new("race");
        res.write("a.note", "hello");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));

        let guids: Vec<AssetGuid> = (0..8)
            .map(|_| {
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let mut assets = AssetManager::new();
                    barrier.wait();
                    let outcome = assets
                        .import(request("a.note", MetadataPolicy::CreateIfMissing))
                        .unwrap();
                    // A second import must accept the asset the first one made.
                    let again = assets
                        .import(request("a.note", MetadataPolicy::CreateIfMissing))
                        .unwrap();
                    assert!(again.already_loaded);
                    outcome.guid
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|thread| thread.join().expect("no import panicked"))
            .collect();

        let written = res.read("a.note.meta").expect("one file was written");
        let on_disk = parse_metadata::<Note>(Path::new("a.note.meta"), written.as_bytes())
            .unwrap()
            .guid;
        assert!(
            guids.iter().all(|guid| *guid == on_disk),
            "{guids:?} vs {on_disk}"
        );
    }

    #[test]
    fn metadata_files_and_unnormalized_paths_are_refused() {
        let mut assets = AssetManager::new();
        assert!(matches!(
            assets.import(request("a.note.meta", MetadataPolicy::ReadIfPresent)),
            Err(AssetImportError::Load(AssetLoadError::Metadata { .. }))
        ));
        assert!(matches!(
            assets.import(request("../a.note", MetadataPolicy::ReadIfPresent)),
            Err(AssetImportError::Load(AssetLoadError::PathNotFound { .. }))
        ));
    }

    /// A version 1 asset pack of `files`, as `pill_assets` writes one.
    // -------------------------------------------------------------------------
    // Reimport
    // -------------------------------------------------------------------------

    /// An edited metadata file changes the loaded value in place: same handle,
    /// same guid, and the slot's content version moves.
    #[test]
    fn reimport_replaces_the_value_in_its_slot() {
        let res = ScratchRes::new("reimport-in-place");
        res.write("a.note", "hello");
        res.write("a.note.meta", &metadata_json(GUID_A, r#"{"volume": 2}"#));
        let mut assets = AssetManager::new();
        let first = assets
            .import(request("a.note", MetadataPolicy::ReadIfPresent))
            .unwrap();
        let version_before = assets.content_version(first.handle).unwrap();

        res.write("a.note.meta", &metadata_json(GUID_A, r#"{"volume": 9}"#));
        let outcome = assets.reimport::<Note>(Path::new("a.note")).unwrap();

        assert!(outcome.replaced);
        assert_eq!(outcome.handle, first.handle);
        assert_eq!(outcome.guid, first.guid);
        assert_eq!(assets.get(first.handle).unwrap().settings.volume, 9);
        assert!(assets.content_version(first.handle).unwrap() > version_before);
        assert_eq!(assets.len::<Note>(), 1);
    }

    /// A source that no longer decodes leaves the previous value in place.
    #[test]
    fn reimport_of_a_broken_source_keeps_the_previous_value() {
        let res = ScratchRes::new("reimport-broken");
        res.write("a.note", "hello");
        let mut assets = AssetManager::new();
        let first = assets
            .import(request("a.note", MetadataPolicy::CreateIfMissing))
            .unwrap();
        let version_before = assets.content_version(first.handle).unwrap();

        res.write("a.note", "broken");
        let error = assets.reimport::<Note>(Path::new("a.note")).unwrap_err();

        assert!(matches!(
            error,
            AssetImportError::Load(AssetLoadError::Decode { .. })
        ));
        assert_eq!(assets.get(first.handle).unwrap().text, "hello");
        assert_eq!(
            assets.content_version(first.handle).unwrap(),
            version_before
        );
    }

    /// A metadata file whose guid was edited is refused, and the loaded asset
    /// keeps its guid and value.
    #[test]
    fn reimport_refuses_a_changed_guid() {
        let res = ScratchRes::new("reimport-guid");
        res.write("a.note", "hello");
        res.write("a.note.meta", &metadata_json(GUID_A, "{}"));
        let mut assets = AssetManager::new();
        let first = assets
            .import(request("a.note", MetadataPolicy::ReadIfPresent))
            .unwrap();

        res.write("a.note.meta", &metadata_json(GUID_B, "{}"));
        let error = assets.reimport::<Note>(Path::new("a.note")).unwrap_err();

        assert!(matches!(
            error,
            AssetImportError::PathBoundToOtherGuid { .. }
        ));
        assert_eq!(assets.guid_of(first.handle), Some(first.guid));
    }

    /// Reimporting a path that is not loaded imports it, and says so.
    #[test]
    fn reimport_of_an_unloaded_path_imports_it() {
        let res = ScratchRes::new("reimport-unloaded");
        res.write("a.note", "hello");
        let mut assets = AssetManager::new();

        let outcome = assets.reimport::<Note>(Path::new("a.note")).unwrap();

        assert!(!outcome.replaced);
        assert_eq!(assets.get(outcome.handle).unwrap().text, "hello");
        // Reimport never writes, even for a fresh import.
        assert!(res.read("a.note.meta").is_none());
    }

    // -------------------------------------------------------------------------
    // Following a move
    // -------------------------------------------------------------------------

    /// A source moved together with its metadata file is renamed in place:
    /// same handle, same guid, no decode.
    #[test]
    fn a_moved_source_is_followed_under_its_guid() {
        let res = ScratchRes::new("follow-move");
        res.write("a.note", "hello");
        let mut assets = AssetManager::new();
        let first = assets
            .import(request("a.note", MetadataPolicy::CreateIfMissing))
            .unwrap();
        let decodes_before = decodes();

        std::fs::create_dir_all(res.root.join("moved")).unwrap();
        std::fs::rename(res.root.join("a.note"), res.root.join("moved/b.note")).unwrap();
        std::fs::rename(
            res.root.join("a.note.meta"),
            res.root.join("moved/b.note.meta"),
        )
        .unwrap();
        let old_name = assets
            .follow_move::<Note>(Path::new("moved/b.note"))
            .unwrap();

        assert_eq!(old_name.as_deref(), Some("a.note"));
        assert_eq!(
            assets.handle_by_name::<Note>("moved/b.note"),
            Some(first.handle)
        );
        assert_eq!(
            assets.handle_by_guid::<Note>(first.guid),
            Some(first.handle)
        );
        assert_eq!(decodes(), decodes_before, "a move decodes nothing");
    }

    /// A copy - the old source is still there - is not a move.
    #[test]
    fn a_copied_source_is_not_followed() {
        let res = ScratchRes::new("follow-copy");
        res.write("a.note", "hello");
        let mut assets = AssetManager::new();
        assets
            .import(request("a.note", MetadataPolicy::CreateIfMissing))
            .unwrap();
        res.write("b.note", "hello");
        std::fs::copy(res.root.join("a.note.meta"), res.root.join("b.note.meta")).unwrap();

        assert_eq!(
            assets.follow_move::<Note>(Path::new("b.note")).unwrap(),
            None
        );
        assert!(assets.handle_by_name::<Note>("a.note").is_some());
    }

    /// A loaded path, a path without metadata, or an unknown guid is no move.
    #[test]
    fn unrelated_paths_are_not_moves() {
        let res = ScratchRes::new("follow-none");
        res.write("a.note", "hello");
        res.write("b.note", "hello");
        res.write("c.note", "hello");
        res.write("c.note.meta", &metadata_json(GUID_B, "{}"));
        let mut assets = AssetManager::new();
        assets
            .import(request("a.note", MetadataPolicy::CreateIfMissing))
            .unwrap();

        for path in ["a.note", "b.note", "c.note"] {
            assert_eq!(
                assets.follow_move::<Note>(Path::new(path)).unwrap(),
                None,
                "{path}"
            );
        }
    }

    // -------------------------------------------------------------------------
    // Settings as JSON
    // -------------------------------------------------------------------------

    #[test]
    fn settings_json_comes_from_the_metadata_file_or_the_defaults() {
        let res = ScratchRes::new("settings-json");
        res.write("a.note", "hello");
        res.write("b.note", "hello");
        res.write("a.note.meta", &metadata_json(GUID_A, r#"{"volume": 3}"#));

        let from_file = settings_json_for::<Note>(Path::new("a.note")).unwrap();
        let defaults = settings_json_for::<Note>(Path::new("b.note")).unwrap();

        assert_eq!(from_file["volume"], 3);
        assert_eq!(from_file["label"], "default");
        assert_eq!(defaults["volume"], 7);
    }

    #[test]
    fn normalizing_settings_fills_defaults_and_refuses_misfits() {
        let normalized =
            normalize_settings_json::<Note>(serde_json::json!({"volume": 4, "extra": true}))
                .unwrap();
        assert_eq!(
            normalized,
            serde_json::json!({"volume": 4, "label": "default"})
        );

        let misfit = normalize_settings_json::<Note>(serde_json::json!({"volume": "loud"}));
        assert!(matches!(misfit, Err(AssetLoadError::Metadata { .. })));
    }

    fn pack_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        use crate::asset_store::{ASSET_PACK_MAGIC, ASSET_PACK_VERSION};
        let index_length: usize = files.iter().map(|(path, _)| 4 + path.len() + 16).sum();
        let mut offset = (8 + 4 + 4 + index_length) as u64;
        let mut pack = Vec::new();
        pack.extend_from_slice(ASSET_PACK_MAGIC);
        pack.extend_from_slice(&ASSET_PACK_VERSION.to_le_bytes());
        pack.extend_from_slice(&(files.len() as u32).to_le_bytes());
        for (path, bytes) in files {
            pack.extend_from_slice(&(path.len() as u32).to_le_bytes());
            pack.extend_from_slice(path.as_bytes());
            pack.extend_from_slice(&offset.to_le_bytes());
            pack.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
            offset += bytes.len() as u64;
        }
        for (_, bytes) in files {
            pack.extend_from_slice(bytes);
        }
        pack
    }
}
