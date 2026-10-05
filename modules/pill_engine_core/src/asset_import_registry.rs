//! Importing assets whose Rust type the caller does not know.
//!
//! # Responsibilities
//!
//! - Hold, per imported asset type, its metadata type name, the source
//!   extensions it accepts and type-erased entry points
//!   ([`ImportRegistry`]).
//! - Import, reimport and read or check the settings of a file by its
//!   extension or type name, for code that sees only paths: the `res` scan,
//!   the file watcher, an editor.
//! - Import every known source under a directory in one call
//!   ([`ImportRegistry::scan`]), reporting what it did.
//! - Hold standalone asset types too - files that are the asset, such as a
//!   `.material` - with the default document a new file of each starts from.
//! - Refuse a second type claiming an extension another type already has.
//! - Never call into a binary that is gone: entries whose registering
//!   generation was retired are skipped and pruned.
//!
//! # Design
//!
//! [`AssetManager::import`] is generic, so the type is fixed where it is
//! called. A caller holding only `textures/a.png` needs the step from an
//! extension to a type, which is what [`World::register_imported_asset`]
//! records: each registration stores function pointers to the import
//! functions monomorphized for that type, in the binary that registered it.
//!
//! Those pointers are the reload hazard `AGENTS.md` warns about. They point
//! into the registering DLL, and that image is unmapped some time after a
//! reload retires it. The registry does not track subjects itself. Instead,
//! each registration's liveness rides on a marker resource the registering
//! generation inserts:
//!
//! - A reload's `init` registers again, which records fresh pointers into the
//!   new image and replaces the marker.
//! - A generation that stops registering a type, or a subject that is
//!   unloaded, loses its claim on the marker. The host drops unclaimed
//!   resources while the image is still mapped, which ends the registration:
//!   an entry only holds a weak reference to the marker's token.
//!
//! Several binaries can register one type: a project calls
//! `pill_master_renderer_data::register` in its own `init`, and so does the
//! data crate's own module. Each keeps its own registration, and the newest
//! live one is used, so a project that stops registering a type falls back to
//! the module's registration instead of losing the type.
//!
//! [`World::register_imported_asset`]: crate::World::register_imported_asset

// Standard library
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

// Current crate
use crate::asset::{AssetGuid, AssetLoadError, AssetLoadResult, AssetManager};
use crate::asset_metadata::{
    normalize_settings_json, settings_json_for, AssetImport, AssetImportError, ImportedAsset,
    MetadataPolicy, MetadataSource, METADATA_EXTENSION,
};
use crate::asset_standalone::{
    default_standalone_document, normalize_standalone_document, standalone_document_json,
    StandaloneAsset,
};
use crate::asset_store::pack_key;
use crate::resource::Resource;

// =============================================================================
// Erased entry points
// =============================================================================

/// What an erased import or reimport did, without the typed handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErasedImportOutcome {
    /// The asset type's metadata type name.
    pub type_name: String,
    /// The asset's name: its normalized path.
    pub name: String,
    /// The asset's guid.
    pub guid: AssetGuid,
    /// Where the guid and settings came from.
    pub metadata: MetadataSource,
    /// For an import: the path was already loaded and nothing was decoded.
    /// For a reimport: the loaded value was replaced in its slot.
    pub previously_loaded: bool,
    /// The asset's content version after the call.
    pub content_version: Option<u64>,
    /// The asset's content version before the call, when it was loaded: a
    /// reimport moves it, an import of a loaded path leaves it.
    pub previous_content_version: Option<u64>,
}

/// Import a path as one asset type.
type ImportFunction = fn(&mut AssetManager, &Path, MetadataPolicy) -> ErasedImportResult;
/// Decode a loaded path again, in place.
type ReimportFunction = fn(&mut AssetManager, &Path) -> ErasedImportResult;
/// The settings a path would use, as JSON.
type SettingsJsonFunction = fn(&Path) -> AssetLoadResult<serde_json::Value>;
/// Settings JSON checked against the type and normalized.
type NormalizeSettingsFunction = fn(serde_json::Value) -> AssetLoadResult<serde_json::Value>;
/// Whether an asset is loaded under a path's name.
type IsLoadedFunction = fn(&AssetManager, &Path) -> bool;
/// Follow a source moved with its metadata file; the old name, if it was one.
type FollowMoveFunction = fn(&mut AssetManager, &Path) -> Result<Option<String>, AssetImportError>;
/// A standalone type's default document, as JSON.
type DefaultDocumentFunction = fn() -> AssetLoadResult<serde_json::Value>;
/// Write a sourced asset's missing metadata file; the guid written, if any.
type EnsureMetadataFunction =
    fn(&AssetManager, &Path) -> Result<Option<AssetGuid>, AssetImportError>;

/// The result of an erased import or reimport.
pub type ErasedImportResult = Result<ErasedImportOutcome, AssetImportError>;

/// One type's erased entry points, all monomorphized in the binary that
/// registered it.
#[derive(Clone, Copy)]
struct ImportFunctions {
    import: ImportFunction,
    reimport: ReimportFunction,
    settings_json: SettingsJsonFunction,
    normalize_settings: NormalizeSettingsFunction,
    follow_move: FollowMoveFunction,
    is_loaded: IsLoadedFunction,
    /// `Some` for a standalone type, whose file is the asset; `None` for a
    /// sourced one.
    default_document: Option<DefaultDocumentFunction>,
    /// `Some` for a sourced type, whose metadata is a sidecar; `None` for a
    /// standalone one, which holds its guid itself.
    ensure_metadata: Option<EnsureMetadataFunction>,
}

impl ImportFunctions {
    /// The entry points for `T`, compiled into the calling binary.
    fn of<T: ImportedAsset>() -> Self {
        Self {
            import: import_erased::<T>,
            reimport: reimport_erased::<T>,
            settings_json: settings_json_for::<T>,
            normalize_settings: normalize_settings_json::<T>,
            follow_move: AssetManager::follow_move::<T>,
            is_loaded: is_loaded::<T>,
            default_document: None,
            ensure_metadata: Some(AssetManager::ensure_metadata::<T>),
        }
    }

    /// The entry points for the standalone type `T`, compiled into the calling
    /// binary. "Settings" are the asset's own document here.
    fn of_standalone<T: StandaloneAsset>() -> Self {
        Self {
            import: import_standalone_erased::<T>,
            reimport: reimport_standalone_erased::<T>,
            settings_json: standalone_document_json::<T>,
            normalize_settings: normalize_standalone_document::<T>,
            follow_move: AssetManager::follow_standalone_move::<T>,
            is_loaded: is_loaded::<T>,
            default_document: Some(default_standalone_document::<T>),
            ensure_metadata: None,
        }
    }
}

/// [`AssetManager::import_standalone`] for `T`; the policy does not apply, as
/// a standalone file is never written by an import.
fn import_standalone_erased<T: StandaloneAsset>(
    assets: &mut AssetManager,
    path: &Path,
    _policy: MetadataPolicy,
) -> ErasedImportResult {
    let outcome = assets.import_standalone::<T>(path)?;
    let content_version = assets.content_version(outcome.handle);
    Ok(ErasedImportOutcome {
        type_name: T::metadata_type_name().to_owned(),
        name: assets
            .name_of(outcome.handle)
            .unwrap_or_default()
            .to_owned(),
        guid: outcome.guid,
        metadata: outcome.metadata,
        previously_loaded: outcome.already_loaded,
        content_version,
        previous_content_version: content_version.filter(|_| outcome.already_loaded),
    })
}

/// [`AssetManager::reimport_standalone`] for `T`.
fn reimport_standalone_erased<T: StandaloneAsset>(
    assets: &mut AssetManager,
    path: &Path,
) -> ErasedImportResult {
    let previous_content_version = pack_key(path)
        .and_then(|name| assets.handle_by_name::<T>(&name))
        .and_then(|handle| assets.content_version(handle));
    let outcome = assets.reimport_standalone::<T>(path)?;
    Ok(ErasedImportOutcome {
        type_name: T::metadata_type_name().to_owned(),
        name: assets
            .name_of(outcome.handle)
            .unwrap_or_default()
            .to_owned(),
        guid: outcome.guid,
        metadata: outcome.metadata,
        previously_loaded: outcome.replaced,
        content_version: assets.content_version(outcome.handle),
        previous_content_version,
    })
}

/// [`AssetManager::import`] for `T`, with the type's default initial settings.
fn import_erased<T: ImportedAsset>(
    assets: &mut AssetManager,
    path: &Path,
    policy: MetadataPolicy,
) -> ErasedImportResult {
    let outcome = assets.import(AssetImport::<T>::new(path, policy))?;
    let content_version = assets.content_version(outcome.handle);
    Ok(ErasedImportOutcome {
        type_name: T::metadata_type_name().to_owned(),
        name: assets
            .name_of(outcome.handle)
            .unwrap_or_default()
            .to_owned(),
        guid: outcome.guid,
        metadata: outcome.metadata,
        previously_loaded: outcome.already_loaded,
        content_version,
        previous_content_version: content_version.filter(|_| outcome.already_loaded),
    })
}

/// Whether a `T` is loaded under `path`'s normalized name.
fn is_loaded<T>(assets: &AssetManager, path: &Path) -> bool
where
    T: crate::asset::Asset + trait_type_map::TraitAccessible<dyn crate::asset::Asset>,
{
    pack_key(path).is_some_and(|name| assets.handle_by_name::<T>(&name).is_some())
}

/// [`AssetManager::reimport`] for `T`.
fn reimport_erased<T: ImportedAsset>(assets: &mut AssetManager, path: &Path) -> ErasedImportResult {
    let previous_content_version = pack_key(path)
        .and_then(|name| assets.handle_by_name::<T>(&name))
        .and_then(|handle| assets.content_version(handle));
    let outcome = assets.reimport::<T>(path)?;
    Ok(ErasedImportOutcome {
        type_name: T::metadata_type_name().to_owned(),
        name: assets
            .name_of(outcome.handle)
            .unwrap_or_default()
            .to_owned(),
        guid: outcome.guid,
        metadata: outcome.metadata,
        previously_loaded: outcome.replaced,
        content_version: assets.content_version(outcome.handle),
        previous_content_version,
    })
}

// =============================================================================
// Errors
// =============================================================================

/// Why an erased import, reimport or settings call did not run.
#[derive(Debug, thiserror::Error)]
pub enum ErasedImportError {
    /// No live registration claims the path's extension.
    #[error("no imported asset type is registered for `.{extension}` (`{path}`)")]
    UnknownExtension {
        /// The path asked for.
        path: String,
        /// Its extension, lowercased.
        extension: String,
    },
    /// The path has no extension to look up.
    #[error("`{path}` has no extension, so no asset type can be chosen for it")]
    NoExtension {
        /// The path asked for.
        path: String,
    },
    /// The path is a metadata file, which is never an asset itself.
    #[error("`{path}` is a metadata file, not an asset source")]
    MetadataFile {
        /// The path asked for.
        path: String,
    },
    /// No live registration has this type name.
    #[error("no imported asset type named `{type_name}` is registered")]
    UnknownType {
        /// The type name asked for.
        type_name: String,
    },
    /// The import itself failed.
    #[error(transparent)]
    Import(#[from] AssetImportError),
    /// Reading or checking settings failed.
    #[error(transparent)]
    Load(#[from] AssetLoadError),
}

/// Why [`ImportRegistry::register`] refused a type.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ImportRegistrationError {
    /// Another live type already imports files with this extension.
    #[error(
        "`{incoming_type}` claims source extension `.{extension}`, which `{registered_type}` already imports"
    )]
    ExtensionClaimedTwice {
        /// The contested extension, lowercased.
        extension: String,
        /// The type that has it.
        registered_type: String,
        /// The type that asked for it.
        incoming_type: String,
    },
}

// =============================================================================
// ImportRegistry
// =============================================================================

/// One binary's registration of a type.
#[derive(Clone)]
struct Registration {
    /// Alive while the registering generation's marker resource is.
    alive: Weak<()>,
    /// The extensions that binary's copy of the type declared.
    extensions: Vec<String>,
    /// Entry points into that binary.
    functions: ImportFunctions,
}

impl Registration {
    fn is_alive(&self) -> bool {
        self.alive.strong_count() > 0
    }
}

/// One imported asset type and every live registration of it, newest last.
#[derive(Clone)]
struct ImportEntry {
    /// The type's metadata type name, owned: a `&'static str` would point
    /// into the registering binary.
    type_name: String,
    registrations: Vec<Registration>,
}

impl ImportEntry {
    /// The newest registration still alive, which is the one to call.
    fn current(&self) -> Option<&Registration> {
        self.registrations
            .iter()
            .rev()
            .find(|registration| registration.is_alive())
    }
}

/// The imported asset types of a world, by metadata type name and source
/// extension.
///
/// Filled by [`World::register_imported_asset`](crate::World::register_imported_asset)
/// in each module's registration; see the module docs for why an entry can
/// outlive neither the binary it points into nor that binary's claim.
///
/// Cloning is cheap - a few entries of weak references and function pointers -
/// and is how host code calls it against the world's
/// [`AssetManager`](crate::AssetManager), which is another resource of the
/// same world. A clone follows the same liveness: a registration retired
/// after the clone was taken is skipped by the clone too.
#[derive(Default, Clone)]
pub struct ImportRegistry {
    entries: Vec<ImportEntry>,
}

impl Resource for ImportRegistry {}

impl ImportRegistry {
    /// Record `T`'s entry points, alive for as long as `alive` has a strong
    /// reference.
    ///
    /// Registering a type again (a reload, or a second binary) adds a newer
    /// registration that takes precedence; the older one is pruned once it
    /// dies.
    ///
    /// # Errors
    ///
    /// [`ImportRegistrationError::ExtensionClaimedTwice`] when another live
    /// type already imports one of `T`'s extensions. Nothing is recorded then.
    pub fn register<T: ImportedAsset>(
        &mut self,
        alive: Weak<()>,
    ) -> Result<(), ImportRegistrationError> {
        self.record(
            T::metadata_type_name(),
            T::SOURCE_EXTENSIONS,
            ImportFunctions::of::<T>(),
            alive,
        )
    }

    /// Record the standalone type `T` under its file extension; otherwise as
    /// [`Self::register`].
    ///
    /// # Errors
    ///
    /// As [`Self::register`].
    pub fn register_standalone<T: StandaloneAsset>(
        &mut self,
        alive: Weak<()>,
    ) -> Result<(), ImportRegistrationError> {
        self.record(
            T::metadata_type_name(),
            &[T::FILE_EXTENSION],
            ImportFunctions::of_standalone::<T>(),
            alive,
        )
    }

    /// The registration both kinds share: the extension check, then the entry.
    fn record(
        &mut self,
        type_name: &str,
        extensions: &[&str],
        functions: ImportFunctions,
        alive: Weak<()>,
    ) -> Result<(), ImportRegistrationError> {
        self.prune();
        let extensions: Vec<String> = extensions
            .iter()
            .map(|extension| extension.trim_start_matches('.').to_ascii_lowercase())
            .collect();

        // Two types sharing an extension would make a file's type depend on
        // registration order; refuse it with both names instead.
        for extension in &extensions {
            if let Some(other) = self.type_for_extension(extension) {
                if other != type_name {
                    return Err(ImportRegistrationError::ExtensionClaimedTwice {
                        extension: extension.clone(),
                        registered_type: other.to_owned(),
                        incoming_type: type_name.to_owned(),
                    });
                }
            }
        }

        let registration = Registration {
            alive,
            extensions,
            functions,
        };
        match self
            .entries
            .iter_mut()
            .find(|entry| entry.type_name == type_name)
        {
            Some(entry) => entry.registrations.push(registration),
            None => self.entries.push(ImportEntry {
                type_name: type_name.to_owned(),
                registrations: vec![registration],
            }),
        }
        Ok(())
    }

    /// Drop every registration whose generation is gone, and every entry left
    /// with none.
    pub fn prune(&mut self) {
        for entry in &mut self.entries {
            entry.registrations.retain(Registration::is_alive);
        }
        self.entries.retain(|entry| !entry.registrations.is_empty());
    }

    /// The type that imports `extension` (without the dot, any case), if a
    /// live registration claims it.
    pub fn type_for_extension(&self, extension: &str) -> Option<&str> {
        let extension = extension.trim_start_matches('.').to_ascii_lowercase();
        self.entries.iter().find_map(|entry| {
            let current = entry.current()?;
            current
                .extensions
                .contains(&extension)
                .then_some(entry.type_name.as_str())
        })
    }

    /// The live types and the extensions each imports, sorted by type name.
    pub fn registered_types(&self) -> Vec<(String, Vec<String>)> {
        let mut types: Vec<(String, Vec<String>)> = self
            .entries
            .iter()
            .filter_map(|entry| {
                let current = entry.current()?;
                Some((entry.type_name.clone(), current.extensions.clone()))
            })
            .collect();
        types.sort();
        types
    }

    /// The live standalone types - those whose file is the asset - with the
    /// one extension each is stored under, sorted by type name.
    pub fn standalone_types(&self) -> Vec<(String, String)> {
        let mut types: Vec<(String, String)> = self
            .entries
            .iter()
            .filter_map(|entry| {
                let current = entry.current()?;
                current.functions.default_document?;
                Some((entry.type_name.clone(), current.extensions.first()?.clone()))
            })
            .collect();
        types.sort();
        types
    }

    /// Whether files with `extension` are standalone assets of a live type.
    pub fn is_standalone_extension(&self, extension: &str) -> bool {
        let extension = extension.trim_start_matches('.').to_ascii_lowercase();
        self.entries.iter().any(|entry| {
            entry.current().is_some_and(|current| {
                current.functions.default_document.is_some()
                    && current.extensions.contains(&extension)
            })
        })
    }

    /// The document a new file of the standalone type `type_name` starts
    /// from, as JSON.
    ///
    /// # Errors
    ///
    /// [`ErasedImportError::UnknownType`] when no live standalone type has
    /// that name.
    pub fn default_document(
        &self,
        type_name: &str,
    ) -> Result<serde_json::Value, ErasedImportError> {
        let functions = self.functions_for_type(type_name)?;
        let default_document =
            functions
                .default_document
                .ok_or_else(|| ErasedImportError::UnknownType {
                    type_name: type_name.to_owned(),
                })?;
        Ok(default_document()?)
    }

    /// Whether `path` is a metadata file rather than a source.
    pub fn is_metadata_file(path: &Path) -> bool {
        path.extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case(METADATA_EXTENSION))
    }

    /// The entry points for the type that imports `path`'s extension.
    fn functions_for_path(&self, path: &Path) -> Result<ImportFunctions, ErasedImportError> {
        let display = path.display().to_string();
        if Self::is_metadata_file(path) {
            return Err(ErasedImportError::MetadataFile { path: display });
        }
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .ok_or_else(|| ErasedImportError::NoExtension {
                path: display.clone(),
            })?
            .to_ascii_lowercase();
        self.entries
            .iter()
            .filter_map(ImportEntry::current)
            .find(|registration| registration.extensions.contains(&extension))
            .map(|registration| registration.functions)
            .ok_or(ErasedImportError::UnknownExtension {
                path: display,
                extension,
            })
    }

    /// The entry points registered under `type_name`.
    fn functions_for_type(&self, type_name: &str) -> Result<ImportFunctions, ErasedImportError> {
        self.entries
            .iter()
            .find(|entry| entry.type_name == type_name)
            .and_then(ImportEntry::current)
            .map(|registration| registration.functions)
            .ok_or_else(|| ErasedImportError::UnknownType {
                type_name: type_name.to_owned(),
            })
    }

    /// Import `path` as the type its extension maps to, with that type's
    /// default initial settings; see [`AssetManager::import`].
    ///
    /// # Errors
    ///
    /// [`ErasedImportError::UnknownExtension`] and its siblings when no type
    /// is chosen, and [`ErasedImportError::Import`] when the import fails.
    pub fn import(
        &self,
        assets: &mut AssetManager,
        path: &Path,
        policy: MetadataPolicy,
    ) -> Result<ErasedImportOutcome, ErasedImportError> {
        let functions = self.functions_for_path(path)?;
        Ok((functions.import)(assets, path, policy)?)
    }

    /// Decode `path` again in place as the type its extension maps to; see
    /// [`AssetManager::reimport`].
    ///
    /// # Errors
    ///
    /// As [`Self::import`].
    pub fn reimport(
        &self,
        assets: &mut AssetManager,
        path: &Path,
    ) -> Result<ErasedImportOutcome, ErasedImportError> {
        let functions = self.functions_for_path(path)?;
        Ok((functions.reimport)(assets, path)?)
    }

    /// Write the metadata file of the source at `path` when it has none; see
    /// [`AssetManager::ensure_metadata`]. Returns the guid written, or `None`
    /// when nothing was written - the file exists, or `path` is a standalone
    /// asset, which has no sidecar.
    ///
    /// # Errors
    ///
    /// As [`Self::import`].
    pub fn ensure_metadata(
        &self,
        assets: &AssetManager,
        path: &Path,
    ) -> Result<Option<AssetGuid>, ErasedImportError> {
        let functions = self.functions_for_path(path)?;
        match functions.ensure_metadata {
            Some(ensure) => Ok(ensure(assets, path)?),
            None => Ok(None),
        }
    }

    /// Give every source under `root` that a registered sourced type imports
    /// a metadata file, where it has none. Nothing is decoded or loaded.
    /// Returns the files written (as their sources' paths) and the failures.
    pub fn ensure_all_metadata(
        &self,
        assets: &AssetManager,
        root: &Path,
    ) -> (Vec<String>, Vec<(String, String)>) {
        let mut written = Vec::new();
        let mut failed = Vec::new();
        for relative in source_files(root) {
            let name = relative.to_string_lossy().replace('\\', "/");
            match self.ensure_metadata(assets, &relative) {
                Ok(Some(_)) => written.push(name),
                Ok(None)
                | Err(ErasedImportError::UnknownExtension { .. })
                | Err(ErasedImportError::NoExtension { .. }) => {}
                Err(error) => failed.push((name, error.to_string())),
            }
        }
        (written, failed)
    }

    /// Whether an asset of the type `path`'s extension maps to is loaded under
    /// `path`'s name. `false` for a path no registered type imports.
    pub fn is_loaded(&self, assets: &AssetManager, path: &Path) -> bool {
        self.functions_for_path(path)
            .is_ok_and(|functions| (functions.is_loaded)(assets, path))
    }

    /// When `path` is a loaded asset moved here with its metadata file, rename
    /// it to `path` and return its old name; see
    /// [`AssetManager::follow_move`].
    ///
    /// # Errors
    ///
    /// As [`Self::import`].
    pub fn follow_move(
        &self,
        assets: &mut AssetManager,
        path: &Path,
    ) -> Result<Option<String>, ErasedImportError> {
        let functions = self.functions_for_path(path)?;
        Ok((functions.follow_move)(assets, path)?)
    }

    /// The import settings `path` would use, as JSON: its metadata file's, or
    /// its type's defaults.
    ///
    /// # Errors
    ///
    /// When no type is chosen for the path, or its metadata file exists but
    /// does not read as that type's.
    pub fn settings_json(&self, path: &Path) -> Result<serde_json::Value, ErasedImportError> {
        let functions = self.functions_for_path(path)?;
        Ok((functions.settings_json)(path)?)
    }

    /// `settings` read as `type_name`'s import settings and written back:
    /// missing fields defaulted, unknown ones dropped.
    ///
    /// # Errors
    ///
    /// When the type is not registered, or the settings do not fit it.
    pub fn normalize_settings_json(
        &self,
        type_name: &str,
        settings: serde_json::Value,
    ) -> Result<serde_json::Value, ErasedImportError> {
        let functions = self.functions_for_type(type_name)?;
        Ok((functions.normalize_settings)(settings)?)
    }
}

// =============================================================================
// Scanning a directory
// =============================================================================

/// What [`ImportRegistry::scan`] did, file by file.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ScanReport {
    /// Assets decoded by this scan, by name.
    pub imported: Vec<String>,
    /// Assets that were already loaded; nothing was decoded for them.
    pub already_loaded: Vec<String>,
    /// The subset of [`Self::imported`] whose metadata file this scan wrote.
    pub metadata_created: Vec<String>,
    /// Sources that failed, by name, with the error.
    pub failed: Vec<(String, String)>,
    /// Extensions no registered type imports, each listed once, sorted; an
    /// empty string stands for files with no extension.
    pub unknown_extensions: Vec<String>,
}

impl ImportRegistry {
    /// Import every source file under `root` whose extension a registered
    /// type imports.
    ///
    /// `root` is the directory asset paths are relative to - a project's
    /// `res` - and each file is imported by its path relative to it, exactly
    /// as loading code names it. Metadata files are skipped (they are read
    /// with their source), as are dot-files such as the temporary files a
    /// metadata write leaves for an instant. A file of an unknown extension is
    /// not an error: the extension is reported once in the result, however
    /// many files carry it.
    ///
    /// A second scan of the same directory decodes and writes nothing: every
    /// path is already loaded, and every file it would write exists.
    pub fn scan(
        &self,
        assets: &mut AssetManager,
        root: &Path,
        policy: MetadataPolicy,
    ) -> ScanReport {
        let mut report = ScanReport::default();
        let mut unknown = BTreeSet::new();
        // Standalone files last: they refer to other assets by guid, and a
        // reference to an asset not loaded yet resolves to nothing.
        let (standalone, sourced): (Vec<PathBuf>, Vec<PathBuf>) =
            source_files(root).into_iter().partition(|path| {
                path.extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| self.is_standalone_extension(extension))
            });
        for relative in sourced.into_iter().chain(standalone) {
            let name = relative.to_string_lossy().replace('\\', "/");
            match self.import(assets, &relative, policy) {
                Ok(outcome) if outcome.previously_loaded => report.already_loaded.push(name),
                Ok(outcome) => {
                    if outcome.metadata == MetadataSource::CreatedOnDisk {
                        report.metadata_created.push(name.clone());
                    }
                    report.imported.push(name);
                }
                Err(ErasedImportError::UnknownExtension { extension, .. }) => {
                    unknown.insert(extension);
                }
                Err(ErasedImportError::NoExtension { .. }) => {
                    unknown.insert(String::new());
                }
                Err(error) => report.failed.push((name, error.to_string())),
            }
        }
        report.unknown_extensions = unknown.into_iter().collect();
        report
    }
}

/// Every file under `root`, as paths relative to it, sorted; metadata files
/// and dot-files left out. An unreadable directory contributes nothing.
fn source_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![PathBuf::new()];
    while let Some(relative_directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(root.join(&relative_directory)) else {
            continue;
        };
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            if file_name.to_string_lossy().starts_with('.') {
                continue;
            }
            let relative = relative_directory.join(&file_name);
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => pending.push(relative),
                Ok(kind) if kind.is_file() && !ImportRegistry::is_metadata_file(&relative) => {
                    files.push(relative);
                }
                _ => {}
            }
        }
    }
    files.sort();
    files
}

// =============================================================================
// The liveness marker
// =============================================================================

/// The resource whose lifetime is one generation's registration of `T`.
///
/// Inserted by [`World::register_imported_asset`](crate::World::register_imported_asset),
/// so it is claimed by the registering generation like any resource it
/// registers. The registry holds only a weak reference to `alive`.
pub(crate) struct ImportRegistrationMarker<T> {
    alive: Arc<()>,
    _type: std::marker::PhantomData<fn() -> T>,
}

impl<T> ImportRegistrationMarker<T> {
    /// A marker with a fresh token.
    pub(crate) fn new() -> Self {
        Self {
            alive: Arc::new(()),
            _type: std::marker::PhantomData,
        }
    }

    /// The weak reference a registration holds.
    pub(crate) fn alive(&self) -> Weak<()> {
        Arc::downgrade(&self.alive)
    }
}

impl<T: 'static> Resource for ImportRegistrationMarker<T> {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::{Asset, AssetLoadResult};
    use crate::asset_store::{mount_directory, mounted_directory_test_lock};
    use crate::World;
    use serde::{Deserialize, Serialize};
    use std::path::PathBuf;
    use trait_type_map::impl_trait_accessible;

    /// Settings of the text test asset.
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    #[serde(default)]
    struct TextSettings {
        uppercase: bool,
    }

    /// A text file as an asset.
    #[derive(Debug)]
    struct Text(String);
    impl Asset for Text {}
    impl_trait_accessible!(dyn Asset; Text);

    impl ImportedAsset for Text {
        type ImportSettings = TextSettings;
        const SOURCE_EXTENSIONS: &'static [&'static str] = &["txt", "TEXT"];

        fn metadata_type_name() -> &'static str {
            "pill_engine::asset_import_registry::tests::Text"
        }

        fn import(_name: &str, bytes: &[u8], settings: &TextSettings) -> AssetLoadResult<Self> {
            let text = String::from_utf8_lossy(bytes).into_owned();
            Ok(Self(if settings.uppercase {
                text.to_uppercase()
            } else {
                text
            }))
        }
    }

    /// A second type, which also wants `.txt`.
    #[derive(Debug)]
    struct Script;
    impl Asset for Script {}
    impl_trait_accessible!(dyn Asset; Script);

    impl ImportedAsset for Script {
        type ImportSettings = TextSettings;
        const SOURCE_EXTENSIONS: &'static [&'static str] = &["lua", "txt"];

        fn metadata_type_name() -> &'static str {
            "pill_engine::asset_import_registry::tests::Script"
        }

        fn import(_name: &str, _bytes: &[u8], _settings: &TextSettings) -> AssetLoadResult<Self> {
            Ok(Self)
        }
    }

    /// A scratch `res` directory mounted under the mounted-directory lock.
    struct ScratchRes {
        root: PathBuf,
        _mounted: std::sync::MutexGuard<'static, ()>,
    }

    impl ScratchRes {
        fn new(label: &str) -> Self {
            let mounted = mounted_directory_test_lock();
            let root = std::env::temp_dir().join(format!(
                "pill-import-registry-{label}-{}",
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
            std::fs::write(self.root.join(relative), contents).unwrap();
        }
    }

    impl Drop for ScratchRes {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn a_file_is_imported_by_its_extension_in_any_case() {
        let res = ScratchRes::new("by-extension");
        res.write("a.TXT", "hello");
        let marker = ImportRegistrationMarker::<Text>::new();
        let mut registry = ImportRegistry::default();
        registry.register::<Text>(marker.alive()).unwrap();
        let mut assets = AssetManager::new();

        let outcome = registry
            .import(
                &mut assets,
                Path::new("a.TXT"),
                MetadataPolicy::CreateIfMissing,
            )
            .unwrap();

        assert_eq!(outcome.type_name, Text::metadata_type_name());
        assert_eq!(outcome.name, "a.TXT");
        assert_eq!(outcome.metadata, MetadataSource::CreatedOnDisk);
        assert_eq!(
            registry.type_for_extension("text"),
            Some(Text::metadata_type_name())
        );
        assert_eq!(assets.get_by_name::<Text>("a.TXT").unwrap().0, "hello");
    }

    #[test]
    fn an_extension_claimed_by_two_types_names_both() {
        let text = ImportRegistrationMarker::<Text>::new();
        let script = ImportRegistrationMarker::<Script>::new();
        let mut registry = ImportRegistry::default();
        registry.register::<Text>(text.alive()).unwrap();

        let error = registry.register::<Script>(script.alive()).unwrap_err();

        assert_eq!(
            error,
            ImportRegistrationError::ExtensionClaimedTwice {
                extension: "txt".to_owned(),
                registered_type: Text::metadata_type_name().to_owned(),
                incoming_type: Script::metadata_type_name().to_owned(),
            }
        );
        // Nothing of the refused type was recorded.
        assert_eq!(registry.type_for_extension("lua"), None);
    }

    /// A registration whose generation was retired is never called, and its
    /// extensions are free for another type.
    #[test]
    fn a_retired_registration_is_skipped_and_pruned() {
        let marker = ImportRegistrationMarker::<Text>::new();
        let mut registry = ImportRegistry::default();
        registry.register::<Text>(marker.alive()).unwrap();
        drop(marker);
        let mut assets = AssetManager::new();

        let error = registry
            .import(
                &mut assets,
                Path::new("a.txt"),
                MetadataPolicy::ReadIfPresent,
            )
            .unwrap_err();
        assert!(matches!(error, ErasedImportError::UnknownExtension { .. }));

        let script = ImportRegistrationMarker::<Script>::new();
        registry.register::<Script>(script.alive()).unwrap();
        assert_eq!(registry.registered_types().len(), 1);
    }

    /// Two binaries registering one type: the newer is used, and when it is
    /// retired the older one takes over.
    #[test]
    fn the_newest_live_registration_wins_and_falls_back() {
        let module = ImportRegistrationMarker::<Text>::new();
        let project = ImportRegistrationMarker::<Text>::new();
        let mut registry = ImportRegistry::default();
        registry.register::<Text>(module.alive()).unwrap();
        registry.register::<Text>(project.alive()).unwrap();
        assert_eq!(registry.registered_types().len(), 1);

        drop(project);
        registry.prune();

        assert_eq!(
            registry.type_for_extension("txt"),
            Some(Text::metadata_type_name())
        );
    }

    #[test]
    fn reimport_and_settings_go_through_the_registry() {
        let res = ScratchRes::new("reimport");
        res.write("a.txt", "hello");
        let marker = ImportRegistrationMarker::<Text>::new();
        let mut registry = ImportRegistry::default();
        registry.register::<Text>(marker.alive()).unwrap();
        let mut assets = AssetManager::new();
        let imported = registry
            .import(
                &mut assets,
                Path::new("a.txt"),
                MetadataPolicy::CreateIfMissing,
            )
            .unwrap();

        let meta = res.root.join("a.txt.meta");
        let edited = std::fs::read_to_string(&meta)
            .unwrap()
            .replace("\"uppercase\": false", "\"uppercase\": true");
        std::fs::write(&meta, edited).unwrap();
        let reimported = registry.reimport(&mut assets, Path::new("a.txt")).unwrap();

        assert!(reimported.previously_loaded);
        assert_eq!(reimported.guid, imported.guid);
        assert!(reimported.content_version > imported.content_version);
        assert_eq!(assets.get_by_name::<Text>("a.txt").unwrap().0, "HELLO");
        assert_eq!(
            registry.settings_json(Path::new("a.txt")).unwrap(),
            serde_json::json!({"uppercase": true})
        );
        assert_eq!(
            registry
                .normalize_settings_json(Text::metadata_type_name(), serde_json::json!({}))
                .unwrap(),
            serde_json::json!({"uppercase": false})
        );
    }

    #[test]
    fn metadata_files_and_paths_without_extensions_are_refused() {
        let registry = ImportRegistry::default();
        let mut assets = AssetManager::new();
        assert!(matches!(
            registry.import(
                &mut assets,
                Path::new("a.txt.meta"),
                MetadataPolicy::ReadIfPresent
            ),
            Err(ErasedImportError::MetadataFile { .. })
        ));
        assert!(matches!(
            registry.import(
                &mut assets,
                Path::new("README"),
                MetadataPolicy::ReadIfPresent
            ),
            Err(ErasedImportError::NoExtension { .. })
        ));
    }

    /// The world-level registration: the marker is a resource of the world,
    /// so removing it - which is what the host does to a resource a retired
    /// generation no longer claims - ends the registration.
    #[test]
    fn the_world_registration_lives_as_long_as_its_marker_resource() {
        let mut world = World::new();
        world.register_imported_asset::<Text>();
        assert!(world.take_registration_error().is_none());
        assert_eq!(
            world
                .get_resource::<ImportRegistry>()
                .unwrap()
                .type_for_extension("txt"),
            Some(Text::metadata_type_name())
        );

        // Registering again, as a reload's `init` does, keeps exactly one
        // live registration.
        world.register_imported_asset::<Text>();
        world.get_resource_mut::<ImportRegistry>().unwrap().prune();
        assert_eq!(
            world.get_resource::<ImportRegistry>().unwrap().entries[0]
                .registrations
                .len(),
            1
        );

        world
            .remove_resource::<ImportRegistrationMarker<Text>>()
            .unwrap();
        assert_eq!(
            world
                .get_resource::<ImportRegistry>()
                .unwrap()
                .type_for_extension("txt"),
            None
        );

        // The reload's re-homing pass drops the dead entry outright.
        world.rehome_assets();
        assert!(world
            .get_resource::<ImportRegistry>()
            .unwrap()
            .entries
            .is_empty());
    }

    /// Known files are imported, a stray metadata file is skipped, and each
    /// unknown extension is reported once however many files carry it.
    #[test]
    fn a_scan_imports_known_files_and_reports_unknown_extensions_once() {
        let res = ScratchRes::new("scan");
        std::fs::create_dir_all(res.root.join("nested/deeper")).unwrap();
        res.write("a.txt", "a");
        res.write("nested/b.txt", "b");
        res.write("nested/deeper/c.TXT", "c");
        res.write("stray.png.meta", "{}");
        res.write("one.bin", "?");
        res.write("two.bin", "?");
        res.write("README", "?");
        res.write(".hidden.tmp", "?");
        let marker = ImportRegistrationMarker::<Text>::new();
        let mut registry = ImportRegistry::default();
        registry.register::<Text>(marker.alive()).unwrap();
        let mut assets = AssetManager::new();

        let first = registry.scan(&mut assets, &res.root, MetadataPolicy::CreateIfMissing);

        assert_eq!(
            first.imported,
            ["a.txt", "nested/b.txt", "nested/deeper/c.TXT"]
        );
        assert_eq!(first.metadata_created, first.imported);
        assert_eq!(first.unknown_extensions, ["", "bin"]);
        assert!(first.failed.is_empty(), "{:?}", first.failed);
        assert!(first.already_loaded.is_empty());
        assert!(res.root.join("nested/deeper/c.TXT.meta").is_file());
        assert_eq!(assets.get_by_name::<Text>("nested/b.txt").unwrap().0, "b");

        // A second scan decodes and creates nothing.
        let second = registry.scan(&mut assets, &res.root, MetadataPolicy::CreateIfMissing);
        assert!(second.imported.is_empty());
        assert!(second.metadata_created.is_empty());
        assert_eq!(second.already_loaded, first.imported);
    }

    /// A source that fails is reported with its error, and the scan goes on.
    #[test]
    fn a_failing_source_is_reported_and_the_scan_continues() {
        let res = ScratchRes::new("scan-failure");
        res.write("a.txt", "a");
        res.write("b.txt", "b");
        res.write("a.txt.meta", "not json");
        let marker = ImportRegistrationMarker::<Text>::new();
        let mut registry = ImportRegistry::default();
        registry.register::<Text>(marker.alive()).unwrap();
        let mut assets = AssetManager::new();

        let report = registry.scan(&mut assets, &res.root, MetadataPolicy::ReadIfPresent);

        assert_eq!(report.imported, ["b.txt"]);
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, "a.txt");
    }

    /// A standalone type: a label that refers to a text asset by guid, and
    /// records whether the reference resolved when it was built.
    #[derive(Debug)]
    struct Label {
        resolved: bool,
    }
    impl Asset for Label {}
    impl_trait_accessible!(dyn Asset; Label);

    #[derive(Default, Serialize, Deserialize)]
    #[serde(default)]
    struct LabelDocument {
        text: crate::asset_reference::AssetReference<Text>,
    }

    impl crate::asset_standalone::StandaloneAsset for Label {
        type Document = LabelDocument;
        const FILE_EXTENSION: &'static str = "label";

        fn metadata_type_name() -> &'static str {
            "pill_engine::asset_import_registry::tests::Label"
        }

        fn from_document(
            _name: &str,
            document: LabelDocument,
            assets: &AssetManager,
        ) -> AssetLoadResult<Self> {
            Ok(Self {
                resolved: document.text.try_resolve(assets).is_some(),
            })
        }
    }

    #[test]
    fn standalone_types_are_listed_with_their_extension_and_default() {
        let text = ImportRegistrationMarker::<Text>::new();
        let label = ImportRegistrationMarker::<Label>::new();
        let mut registry = ImportRegistry::default();
        registry.register::<Text>(text.alive()).unwrap();
        registry
            .register_standalone::<Label>(label.alive())
            .unwrap();

        assert_eq!(
            registry.standalone_types(),
            [(Label::metadata_type_name().to_owned(), "label".to_owned())]
        );
        assert!(registry.is_standalone_extension("LABEL"));
        assert!(!registry.is_standalone_extension("txt"));
        assert_eq!(
            registry
                .default_document(Label::metadata_type_name())
                .unwrap(),
            serde_json::json!({"text": null})
        );
        // A sourced type has no default document.
        assert!(registry
            .default_document(Text::metadata_type_name())
            .is_err());
    }

    /// A scan imports standalone files after every sourced one, so a label
    /// whose name sorts first still finds the text it refers to.
    #[test]
    fn a_scan_imports_standalone_files_after_their_sources() {
        let res = ScratchRes::new("scan-standalone");
        res.write("z.txt", "z");
        let text = ImportRegistrationMarker::<Text>::new();
        let label = ImportRegistrationMarker::<Label>::new();
        let mut registry = ImportRegistry::default();
        registry.register::<Text>(text.alive()).unwrap();
        registry
            .register_standalone::<Label>(label.alive())
            .unwrap();

        // The text's guid has to be known up front for the label to name it.
        let mut probe = AssetManager::new();
        let guid = registry
            .import(
                &mut probe,
                Path::new("z.txt"),
                MetadataPolicy::CreateIfMissing,
            )
            .unwrap()
            .guid;
        res.write(
            "a.label",
            &format!(
                r#"{{"format_version": 1, "asset_type": "{}", "guid": "{}", "asset": {{"text": "{guid}"}}}}"#,
                Label::metadata_type_name(),
                "0000000000000000000000000000001a"
            ),
        );
        let mut assets = AssetManager::new();

        let report = registry.scan(&mut assets, &res.root, MetadataPolicy::CreateIfMissing);

        assert_eq!(report.imported, ["z.txt", "a.label"]);
        assert!(assets.get_by_name::<Label>("a.label").unwrap().resolved);
        assert!(
            !res.root.join("a.label.meta").exists(),
            "a standalone file gets no sidecar"
        );
    }

    /// The sweep writes a `.meta` for each source without one, skips sources
    /// that have one, unknown extensions and standalone files, and loads
    /// nothing.
    #[test]
    fn the_sweep_pairs_every_source_with_metadata() {
        let res = ScratchRes::new("ensure-sweep");
        std::fs::create_dir_all(res.root.join("nested")).unwrap();
        res.write("a.txt", "a");
        res.write("nested/b.txt", "b");
        res.write("other.bin", "?");
        res.write("note.label", "{}");
        let text = ImportRegistrationMarker::<Text>::new();
        let label = ImportRegistrationMarker::<Label>::new();
        let mut registry = ImportRegistry::default();
        registry.register::<Text>(text.alive()).unwrap();
        registry
            .register_standalone::<Label>(label.alive())
            .unwrap();
        let assets = AssetManager::new();

        let (written, failed) = registry.ensure_all_metadata(&assets, &res.root);

        assert_eq!(written, ["a.txt", "nested/b.txt"]);
        assert!(failed.is_empty(), "{failed:?}");
        assert!(res.root.join("nested/b.txt.meta").is_file());
        assert!(
            !res.root.join("note.label.meta").exists(),
            "a standalone file has no sidecar"
        );
        assert_eq!(assets.len::<Text>(), 0, "nothing was loaded");
        let (again, _) = registry.ensure_all_metadata(&assets, &res.root);
        assert!(again.is_empty());
    }

    #[test]
    fn a_world_registration_conflict_is_a_registration_error() {
        let mut world = World::new();
        world.register_imported_asset::<Text>();
        world.register_imported_asset::<Script>();
        let error = world
            .take_registration_error()
            .expect("the conflict is recorded");
        let message = error.to_string();
        assert!(message.contains(Text::metadata_type_name()), "{message}");
        assert!(message.contains(Script::metadata_type_name()), "{message}");
    }
}
