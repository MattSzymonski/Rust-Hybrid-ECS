//! The C ABI of importing an asset through its metadata file, shared by every
//! crate that offers an import export to the C# bridge.
//!
//! # Responsibilities
//!
//! - Define the output a managed import receives ([`NativeImportedAsset`]) and
//!   the status codes it can return ([`import_status`]).
//! - Run [`AssetManager::import`] for one asset type from raw C arguments
//!   ([`import_for_ffi`]): path, policy, and initial settings as UTF-8 JSON.
//!
//! # Design
//!
//! Each asset type keeps its own named export - `pill_render_data_import_texture`,
//! `pill_audio_import_sound`, ... - in the crate that owns the type, because
//! the export is where the type's monomorphized `import` is compiled
//! (`AGENTS.md`: one named export per function, never a dispatcher taking a
//! kind). What those exports share - reading the arguments, folding errors
//! into status codes, writing the output - lives here once, generic over the
//! type, so each export is a single call.
//!
//! Settings cross the boundary as JSON bytes, so no settings struct needs a
//! `repr(C)` mirror on the managed side; an empty buffer means the type's
//! defaults. The full error is logged where it happens, and the status code
//! names its kind for the managed exception.

// Standard library
use std::path::Path;

// Current crate
use crate::asset::{AssetLoadError, AssetManager};
use crate::asset_metadata::{
    AssetImport, AssetImportError, ImportedAsset, MetadataPolicy, MetadataSource,
};
use crate::asset_standalone::StandaloneAsset;
use crate::world::World;

/// Status codes an import export returns. `0` is success; `1` to `7` are the
/// codes the renderer data crate's other asset functions already use, so the
/// managed side's one table covers both.
pub mod import_status {
    /// The asset was imported or was already loaded.
    pub const OK: u8 = 0;
    /// No managed invocation is active: the world was null.
    pub const NO_ACTIVE_SCOPE: u8 = 1;
    /// The world has no `AssetManager` resource.
    pub const ASSET_MANAGER_MISSING: u8 = 2;
    /// The path or the settings were not valid UTF-8.
    pub const INVALID_UTF8: u8 = 3;
    /// The source failed to decode with its settings.
    pub const DECODE_FAILED: u8 = 4;
    /// A required buffer or output was null.
    pub const NULL_OUTPUT: u8 = 5;
    /// No source file exists at the path, in any mount.
    pub const SOURCE_NOT_FOUND: u8 = 8;
    /// The metadata file exists but cannot be read as this type's, or could
    /// not be written.
    pub const METADATA_INVALID: u8 = 9;
    /// The path is loaded under a different guid than its metadata file holds.
    pub const PATH_BOUND_TO_OTHER_GUID: u8 = 10;
    /// The metadata file's guid belongs to another loaded asset.
    pub const DUPLICATE_GUID: u8 = 11;
    /// The initial settings JSON does not fit the type's settings.
    pub const INVALID_SETTINGS: u8 = 12;
    /// The policy byte is not one of the known policies.
    pub const INVALID_POLICY: u8 = 13;
}

/// What an import export writes for the managed caller.
///
/// The managed mirror (`NativeImportedAsset` in the C# runtime) reproduces
/// this layout field for field.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct NativeImportedAsset {
    /// The asset handle's slot index.
    pub index: u32,
    /// The asset handle's generation.
    pub generation: u32,
    /// The low 64 bits of the asset's guid.
    pub guid_low: u64,
    /// The high 64 bits of the asset's guid.
    pub guid_high: u64,
    /// `1` when the path was already loaded and its handle was returned.
    pub already_loaded: u8,
    /// Where the guid came from: `0` read from a file, `1` a file was
    /// written, `2` in memory only.
    pub metadata_source: u8,
}

/// Import the asset at `path` as a `T`, from raw C arguments, and write the
/// result to `output`.
///
/// `policy` is `0` for [`MetadataPolicy::ReadIfPresent`] and `1` for
/// [`MetadataPolicy::CreateIfMissing`]. `settings` is the initial settings as
/// UTF-8 JSON, used only when no metadata file exists; an empty buffer means
/// the type's defaults.
///
/// Returns a code from [`import_status`]. Every failure past argument checking
/// is logged with its full message.
///
/// # Safety
///
/// `world` must be null or point at a live `World` no one else uses for the
/// call's duration. `path` and `settings` must reference their declared lengths
/// in readable memory (unless the length is zero), and `output` must be null
/// or writable.
pub unsafe fn import_for_ffi<T: ImportedAsset>(
    world: *mut World,
    path: *const u8,
    path_length: u32,
    policy: u8,
    settings: *const u8,
    settings_length: u32,
    output: *mut NativeImportedAsset,
) -> u8 {
    if output.is_null() {
        return import_status::NULL_OUTPUT;
    }
    let policy = match policy {
        0 => MetadataPolicy::ReadIfPresent,
        1 => MetadataPolicy::CreateIfMissing,
        _ => return import_status::INVALID_POLICY,
    };
    // SAFETY: forwarded from this function's contract.
    let path = match unsafe { read_utf8(path, path_length) } {
        Ok(path) => path,
        Err(status) => return status,
    };
    // SAFETY: forwarded from this function's contract.
    let settings = match unsafe { read_utf8(settings, settings_length) } {
        Ok(settings) => settings,
        Err(status) => return status,
    };
    let mut request = AssetImport::<T>::new(path.as_str(), policy);
    if !settings.trim().is_empty() {
        match serde_json::from_str(&settings) {
            Ok(initial) => request = request.with_initial_settings(initial),
            Err(error) => {
                pill_core::warn!(
                    "import of `{path}`: the initial settings do not fit {}: {error}",
                    T::metadata_type_name()
                );
                return import_status::INVALID_SETTINGS;
            }
        }
    }

    // SAFETY: the caller's contract: null, or a live world used exclusively.
    let Some(world) = (unsafe { world.as_mut() }) else {
        return import_status::NO_ACTIVE_SCOPE;
    };
    let Some(assets) = world.get_resource_mut::<AssetManager>() else {
        return import_status::ASSET_MANAGER_MISSING;
    };
    match assets.import(request) {
        Ok(outcome) => {
            let guid = outcome.guid.value();
            // SAFETY: checked non-null above; the caller's contract makes it
            // writable.
            unsafe {
                *output = NativeImportedAsset {
                    index: outcome.handle.index(),
                    generation: outcome.handle.generation(),
                    guid_low: guid as u64,
                    guid_high: (guid >> 64) as u64,
                    already_loaded: u8::from(outcome.already_loaded),
                    metadata_source: match outcome.metadata {
                        MetadataSource::ReadFromFile => 0,
                        MetadataSource::CreatedOnDisk => 1,
                        MetadataSource::InMemoryOnly => 2,
                    },
                };
            }
            import_status::OK
        }
        Err(error) => {
            pill_core::warn!("import of `{}` failed: {error}", Path::new(&path).display());
            status_of(&error)
        }
    }
}

/// Import the standalone asset at `path` as a `T`, from raw C arguments, and
/// write the result to `output`.
///
/// Standalone assets have no metadata file - the file *is* the asset, its guid
/// in its own header - so there is no policy and no initial settings to pass.
/// The output is the same [`NativeImportedAsset`] shape [`import_for_ffi`]
/// writes, and the status codes are the same [`import_status`] ones.
///
/// # Safety
///
/// `world` must be null or point at a live `World` no one else uses for the
/// call's duration. `path` must reference its declared length in readable
/// memory (unless the length is zero), and `output` must be null or writable.
pub unsafe fn import_standalone_for_ffi<T: StandaloneAsset>(
    world: *mut World,
    path: *const u8,
    path_length: u32,
    output: *mut NativeImportedAsset,
) -> u8 {
    if output.is_null() {
        return import_status::NULL_OUTPUT;
    }
    // SAFETY: forwarded from this function's contract.
    let path = match unsafe { read_utf8(path, path_length) } {
        Ok(path) => path,
        Err(status) => return status,
    };

    // SAFETY: the caller's contract: null, or a live world used exclusively.
    let Some(world) = (unsafe { world.as_mut() }) else {
        return import_status::NO_ACTIVE_SCOPE;
    };
    let Some(assets) = world.get_resource_mut::<AssetManager>() else {
        return import_status::ASSET_MANAGER_MISSING;
    };
    match assets.import_standalone::<T>(Path::new(&path)) {
        Ok(outcome) => {
            let guid = outcome.guid.value();
            // SAFETY: checked non-null above; the caller's contract makes it
            // writable.
            unsafe {
                *output = NativeImportedAsset {
                    index: outcome.handle.index(),
                    generation: outcome.handle.generation(),
                    guid_low: guid as u64,
                    guid_high: (guid >> 64) as u64,
                    already_loaded: u8::from(outcome.already_loaded),
                    metadata_source: match outcome.metadata {
                        MetadataSource::ReadFromFile => 0,
                        MetadataSource::CreatedOnDisk => 1,
                        MetadataSource::InMemoryOnly => 2,
                    },
                };
            }
            import_status::OK
        }
        Err(error) => {
            pill_core::warn!("import of `{}` failed: {error}", Path::new(&path).display());
            status_of(&error)
        }
    }
}

/// The status code an import error is reported as.
fn status_of(error: &AssetImportError) -> u8 {
    match error {
        AssetImportError::Load(AssetLoadError::PathNotFound { .. }) => {
            import_status::SOURCE_NOT_FOUND
        }
        AssetImportError::Load(AssetLoadError::Decode { .. }) => import_status::DECODE_FAILED,
        AssetImportError::Load(AssetLoadError::Utf8 { .. }) => import_status::INVALID_UTF8,
        AssetImportError::Load(_) => import_status::METADATA_INVALID,
        AssetImportError::PathBoundToOtherGuid { .. } => import_status::PATH_BOUND_TO_OTHER_GUID,
        AssetImportError::DuplicateGuid { .. } => import_status::DUPLICATE_GUID,
    }
}

/// `length` bytes at `pointer` as an owned UTF-8 string; empty for a zero
/// length.
///
/// # Safety
///
/// `pointer` must reference `length` readable bytes unless `length` is zero.
unsafe fn read_utf8(pointer: *const u8, length: u32) -> Result<String, u8> {
    if length == 0 {
        return Ok(String::new());
    }
    if pointer.is_null() {
        return Err(import_status::NULL_OUTPUT);
    }
    // SAFETY: the caller's contract.
    let bytes = unsafe { std::slice::from_raw_parts(pointer, length as usize) };
    String::from_utf8(bytes.to_vec()).map_err(|_| import_status::INVALID_UTF8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::{Asset, AssetLoadResult};
    use crate::asset_store::{mount_directory, mounted_directory_test_lock};
    use serde::{Deserialize, Serialize};
    use trait_type_map::impl_trait_accessible;

    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    #[serde(default)]
    struct BlobSettings {
        scale: u32,
    }

    #[derive(Debug)]
    struct Blob {
        scale: u32,
    }
    impl Asset for Blob {}
    impl_trait_accessible!(dyn Asset; Blob);

    impl ImportedAsset for Blob {
        type ImportSettings = BlobSettings;
        const SOURCE_EXTENSIONS: &'static [&'static str] = &["blob"];

        fn metadata_type_name() -> &'static str {
            "pill_engine::asset_ffi::tests::Blob"
        }

        fn import(name: &str, bytes: &[u8], settings: &BlobSettings) -> AssetLoadResult<Self> {
            if bytes == b"broken" {
                return Err(AssetLoadError::Decode {
                    label: name.to_owned(),
                    detail: "broken".to_owned(),
                });
            }
            Ok(Self {
                scale: settings.scale,
            })
        }
    }

    /// Calls `import_for_ffi` the way a C caller does.
    fn call(
        world: &mut World,
        path: &str,
        policy: u8,
        settings: &str,
    ) -> (u8, NativeImportedAsset) {
        let mut output = NativeImportedAsset::default();
        // SAFETY: every pointer is a live local for the call's duration.
        let status = unsafe {
            import_for_ffi::<Blob>(
                world,
                path.as_ptr(),
                path.len() as u32,
                policy,
                settings.as_ptr(),
                settings.len() as u32,
                &mut output,
            )
        };
        (status, output)
    }

    #[test]
    fn an_import_writes_the_handle_and_guid_and_a_second_one_reports_loaded() {
        let _mounted = mounted_directory_test_lock();
        let root = std::env::temp_dir().join(format!("pill-asset-ffi-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.blob"), b"ok").unwrap();
        std::fs::write(root.join("bad.blob"), b"broken").unwrap();
        mount_directory(&root);
        let mut world = World::new();
        world.insert_resource(AssetManager::new());

        let (status, first) = call(&mut world, "a.blob", 1, r#"{"scale": 3}"#);
        assert_eq!(status, import_status::OK);
        assert_eq!(first.already_loaded, 0);
        assert_eq!(first.metadata_source, 1, "the metadata file was written");
        let guid = u128::from(first.guid_low) | (u128::from(first.guid_high) << 64);
        let assets = world.get_resource::<AssetManager>().unwrap();
        let handle = assets.handle_by_name::<Blob>("a.blob").unwrap();
        assert_eq!(
            (first.index, first.generation),
            (handle.index(), handle.generation())
        );
        assert_eq!(assets.guid_of(handle).unwrap().value(), guid);
        assert_eq!(assets.get(handle).unwrap().scale, 3);

        let (status, second) = call(&mut world, "a.blob", 1, "");
        assert_eq!(status, import_status::OK);
        assert_eq!(second.already_loaded, 1);
        assert_eq!(
            (second.guid_low, second.guid_high),
            (first.guid_low, first.guid_high)
        );

        assert_eq!(
            call(&mut world, "missing.blob", 0, "").0,
            import_status::SOURCE_NOT_FOUND
        );
        assert_eq!(
            call(&mut world, "bad.blob", 0, "").0,
            import_status::DECODE_FAILED
        );
        assert_eq!(
            call(&mut world, "b.blob", 0, "not json").0,
            import_status::INVALID_SETTINGS
        );
        assert_eq!(
            call(&mut world, "a.blob", 7, "").0,
            import_status::INVALID_POLICY
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_null_world_or_output_is_reported() {
        let mut output = NativeImportedAsset::default();
        // SAFETY: a null world is part of the contract; the path is a literal.
        let status = unsafe {
            import_for_ffi::<Blob>(
                std::ptr::null_mut(),
                b"a.blob".as_ptr(),
                6,
                0,
                std::ptr::null(),
                0,
                &mut output,
            )
        };
        assert_eq!(status, import_status::NO_ACTIVE_SCOPE);
        let mut world = World::new();
        // SAFETY: as above, with a null output.
        let status = unsafe {
            import_for_ffi::<Blob>(
                &mut world,
                b"a.blob".as_ptr(),
                6,
                0,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(status, import_status::NULL_OUTPUT);
    }
}
