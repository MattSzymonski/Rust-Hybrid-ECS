//! The [`Sound`] asset: an audio file held as its undecoded bytes.
//!
//! # Responsibilities
//!
//! - Declares [`Sound`], stored in the
//!   [`AssetManager`](pill_engine::AssetManager) and addressed by
//!   `Handle<Sound>`.
//! - Validates an audio file's format and reads it through the asset store
//!   ([`Sound::load`]), so a packed sound loads like one on disk.
//! - Imports a sound through its `.meta` file
//!   ([`ImportedAsset`](pill_engine::ImportedAsset), with
//!   [`SoundImportSettings`]), which gives it a stable guid.
//! - Hands the playback path a fresh decoder over those bytes.
//!
//! # Design
//!
//! A sound is an asset rather than a resource because a world holds many of
//! them and the type alone names none - the distinction `pill_engine` draws
//! between [`Resource`](pill_engine::Resource) and [`Asset`](pill_engine::Asset).
//!
//! The bytes stay encoded. A decoder is a one-shot cursor, so two entities
//! playing one sound need two of them, and holding decoded PCM instead would
//! cost roughly an order of magnitude more memory per sound.

// External crates
use pill_core::utils::AssetPathError;
use pill_engine::{Asset, AssetLoadError, AssetLoadResult};
use serde::{Deserialize, Serialize};

// =============================================================================
// Constants
// =============================================================================

/// Audio container formats [`Sound::load`] accepts.
///
/// Checked before any bytes are read, so a typo in a path fails with the path
/// and the allowed list rather than as a decoder error thousands of samples
/// later.
pub const SUPPORTED_AUDIO_FORMATS: &[&str] = &["mp3", "wav", "ogg", "flac"];

// =============================================================================
// Sound
// =============================================================================

/// One loaded audio file, held as its undecoded bytes.
///
/// Stored in the [`AssetManager`](pill_engine::AssetManager) and addressed by
/// `Handle<Sound>`. The bytes are kept encoded and decoded afresh for each
/// playback: a decoder is a one-shot cursor, so two entities playing one sound
/// need two of them, and holding PCM instead would cost roughly an order of
/// magnitude more memory per sound.
///
/// # Examples
///
/// ```no_run
/// # use pill_audio::Sound;
/// # fn demo(assets: &mut pill_engine::AssetManager) -> Result<(), pill_audio::SoundLoadError> {
/// let sound = Sound::load(std::path::Path::new("assets/footstep.wav"))?;
/// let handle = assets.add_named("footstep", sound).expect("a fresh name");
/// # Ok(())
/// # }
/// ```
///
/// `Debug` prints the path and the byte count rather than the bytes: a sound
/// is megabytes of samples, and a default derive would dump all of them into
/// a log line.
///
/// Mirrored to C# as `pill_audio.Sound`: a managed project imports one with
/// `assets.Import<Sound>(path, ...)` and builds one with `Sound.Load` or
/// `Sound.FromBytes`, exactly as a Rust project would.
#[pill_engine::pill_mirror_object(asset, import)]
pub struct Sound {
    /// Path the bytes were read from, kept for diagnostics and reloading.
    path: std::path::PathBuf,
    /// The file's bytes, exactly as they were on disk.
    bytes: std::sync::Arc<[u8]>,
}

impl Asset for Sound {}
trait_type_map::impl_trait_accessible!(dyn Asset; Sound);

impl std::fmt::Debug for Sound {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Sound")
            .field("path", &self.path)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

/// How a sound file is read, as stored in its `.meta` file.
///
/// Empty for now: a sound's bytes say everything about it. It exists so that
/// sound files get metadata files, and with them a guid that survives a
/// rename. A field added later must have a default, so existing files still
/// read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SoundImportSettings {}

impl pill_engine::ImportedAsset for Sound {
    type ImportSettings = SoundImportSettings;
    const SOURCE_EXTENSIONS: &'static [&'static str] = SUPPORTED_AUDIO_FORMATS;

    // `Sound` has no shared name, so the default would be its Rust type path,
    // which changes if the type moves; the files must keep matching.
    fn metadata_type_name() -> &'static str {
        "pill_audio::Sound"
    }

    fn import(
        name: &str,
        source_bytes: &[u8],
        _settings: &SoundImportSettings,
    ) -> AssetLoadResult<Self> {
        let path = std::path::Path::new(name);
        validate_format(path).map_err(|error| AssetLoadError::Decode {
            label: name.to_owned(),
            detail: error.to_string(),
        })?;
        Ok(Self::from_bytes(path, source_bytes.to_vec()))
    }
}

/// Why [`Sound::load`] could not produce a sound.
#[derive(Debug)]
pub enum SoundLoadError {
    /// No mount has the path, or its extension is not a supported format.
    InvalidPath(pill_core::utils::AssetPathError),
    /// The file exists but could not be read.
    Unreadable {
        /// The path that failed.
        path: std::path::PathBuf,
        /// The underlying IO failure.
        source: std::io::Error,
    },
}

impl std::fmt::Display for SoundLoadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPath(error) => write!(formatter, "{error}"),
            Self::Unreadable { path, source } => {
                write!(formatter, "failed to read `{}`: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for SoundLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidPath(error) => Some(error),
            Self::Unreadable { source, .. } => Some(source),
        }
    }
}

#[pill_engine::pill_mirror_impl]
impl Sound {
    /// Read an audio file into memory.
    ///
    /// The path is read through [`asset_store`](pill_engine::asset_store),
    /// like every other asset: a relative path is found in a mounted pack
    /// first (a shipping or web build), then below the project's `res`.
    /// Its extension is checked against [`SUPPORTED_AUDIO_FORMATS`] before
    /// anything is read, so an unsupported format is reported as such rather
    /// than as a decode failure.
    ///
    /// # Errors
    ///
    /// [`SoundLoadError::InvalidPath`] when the format is not supported or no
    /// mount has the path, and [`SoundLoadError::Unreadable`] when the file
    /// was found but cannot be read.
    #[pill_engine::pill_mirror_method]
    pub fn load(path: &std::path::Path) -> Result<Self, SoundLoadError> {
        let path = path.to_path_buf();
        validate_format(&path).map_err(SoundLoadError::InvalidPath)?;

        let bytes = pill_engine::asset_store::read(&path).map_err(|error| match error {
            AssetLoadError::Read { path, source } => SoundLoadError::Unreadable { path, source },
            // `read` fails only as not found or unreadable; anything else is
            // reported as a path no mount could serve, too.
            _ => SoundLoadError::InvalidPath(AssetPathError::InvalidPath {
                path: path.display().to_string(),
            }),
        })?;

        Ok(Self {
            path,
            bytes: bytes.into(),
        })
    }

    /// Build a sound from bytes already in memory.
    ///
    /// For an embedded asset (`include_bytes!`) or one produced by a cooking
    /// step, where there is no path to validate.
    #[pill_engine::pill_mirror_method]
    pub fn from_bytes(path: &std::path::Path, bytes: Vec<u8>) -> Self {
        Self {
            path: path.to_path_buf(),
            bytes: bytes.into(),
        }
    }

    /// The path these bytes were read from.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// The encoded bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Build a fresh decoder over this sound's bytes.
    ///
    /// One decoder is one playback: it is consumed as a sink drains it, so a
    /// second concurrent play needs a second call. The `Arc` is cloned rather
    /// than the bytes, so the copy is a refcount bump regardless of file size.
    ///
    /// Returns `None` when the bytes are not a format rodio can decode - which
    /// [`Sound::load`] cannot rule out, since it checks only the extension.
    pub fn decoder(&self) -> Option<rodio::Decoder<std::io::Cursor<ArcBytes>>> {
        rodio::Decoder::new(std::io::Cursor::new(ArcBytes(self.bytes.clone()))).ok()
    }
}

/// Check that `path` names a format in [`SUPPORTED_AUDIO_FORMATS`].
///
/// Only the extension is checked, never whether a file exists: a packed sound
/// has no file on disk. Whether the path exists is answered by the read.
pub(crate) fn validate_format(path: &std::path::Path) -> Result<(), AssetPathError> {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some(extension) if SUPPORTED_AUDIO_FORMATS.contains(&extension) => Ok(()),
        Some(extension) => Err(AssetPathError::InvalidFormat {
            extension: extension.to_string(),
            allowed: SUPPORTED_AUDIO_FORMATS.join(", "),
        }),
        None => Err(AssetPathError::InvalidPath {
            path: path.display().to_string(),
        }),
    }
}

/// `Arc<[u8]>` that reads as a byte slice, so a decoder can borrow the sound's
/// bytes instead of copying them.
///
/// `std::io::Cursor` needs its inner value to be `AsRef<[u8]>`, and the blanket
/// impls do not cover `Arc<[u8]>`. Wrapping is cheaper than the alternative the
/// old engine used, which copied the whole buffer per playback.
pub struct ArcBytes(std::sync::Arc<[u8]>);

impl AsRef<[u8]> for ArcBytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use pill_engine::asset_store::AssetPack;
    use pill_engine::{AssetImport, AssetManager, ImportedAsset, MetadataPolicy, MetadataSource};

    /// A missing file is reported as an invalid path, before any read.
    #[test]
    fn loading_a_missing_file_fails_with_the_path() {
        let error = Sound::load(std::path::Path::new("definitely/not/here.wav")).unwrap_err();
        assert!(matches!(error, SoundLoadError::InvalidPath(_)));
    }

    /// An unsupported extension is rejected as a format problem rather than
    /// surfacing later as a decode failure, even for a file that exists.
    #[test]
    fn loading_an_unsupported_format_is_rejected() {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let existing = manifest.join("Cargo.toml");
        assert!(existing.exists(), "the crate manifest should exist");

        match Sound::load(&existing).unwrap_err() {
            SoundLoadError::InvalidPath(pill_core::utils::AssetPathError::InvalidFormat {
                extension,
                ..
            }) => assert_eq!(extension, "toml"),
            other => panic!("expected a format rejection, got {other:?}"),
        }
    }

    /// A file on the filesystem loads, given as an absolute path.
    #[test]
    fn a_sound_on_disk_loads() {
        let directory =
            std::env::temp_dir().join(format!("pill_audio_disk_{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let file = directory.join("click.wav");
        std::fs::write(&file, [1, 2, 3, 4]).unwrap();

        let sound = Sound::load(&file).expect("the file exists");
        assert_eq!(sound.bytes(), &[1, 2, 3, 4]);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    /// A sound that exists only in a mounted pack loads, which is how a
    /// shipping or web build reads it: nothing is on disk at that path.
    #[test]
    fn a_sound_found_only_in_a_mounted_pack_loads() {
        // The path is unique to this test, so the pack, which stays mounted
        // for the rest of the process, cannot shadow another test's file.
        let packed_path = "pill_audio_pack_test/only_in_pack.wav";
        let pack = AssetPack::parse(pack_of(&[(packed_path, &[9, 8, 7])])).expect("a valid pack");
        pill_engine::asset_store::mount_pack(pack);

        let path = std::path::Path::new(packed_path);
        assert!(!path.exists() && pill_engine::asset_store::locate_file(path).is_none());
        let sound = Sound::load(path).expect("the pack has the file");
        assert_eq!(sound.bytes(), &[9, 8, 7]);
        assert_eq!(sound.path(), path);
    }

    /// A version 1 pack of `files`, in the layout `asset_store` documents.
    fn pack_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        let index_length: usize = files.iter().map(|(path, _)| 4 + path.len() + 16).sum();
        let mut offset = (8 + 4 + 4 + index_length) as u64;
        let mut pack = Vec::new();
        pack.extend_from_slice(pill_engine::asset_store::ASSET_PACK_MAGIC);
        pack.extend_from_slice(&pill_engine::asset_store::ASSET_PACK_VERSION.to_le_bytes());
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

    /// Importing a sound with `CreateIfMissing` writes its `.meta`; a later
    /// run reads the same guid back, and importing again in the same run (a
    /// project reload) returns the loaded handle.
    ///
    /// The only test in this crate that mounts a directory, so it needs no
    /// lock against another test replacing the mount.
    #[test]
    fn importing_a_sound_writes_its_metadata_and_keeps_its_guid() {
        let root = std::env::temp_dir().join(format!("pill_audio_import_{}", std::process::id()));
        std::fs::create_dir_all(root.join("sounds")).unwrap();
        std::fs::write(root.join("sounds/click.mp3"), [1, 2, 3]).unwrap();
        pill_engine::asset_store::mount_directory(&root);
        let request =
            || AssetImport::<Sound>::new("sounds/click.mp3", MetadataPolicy::CreateIfMissing);

        let mut first_run = AssetManager::new();
        let created = first_run.import(request()).expect("the file exists");
        assert_eq!(created.metadata, MetadataSource::CreatedOnDisk);
        let metadata = std::fs::read_to_string(root.join("sounds/click.mp3.meta")).unwrap();
        assert!(metadata.contains("\"pill_audio::Sound\""), "{metadata}");
        assert_eq!(
            first_run.get(created.handle).map(Sound::bytes),
            Some(&[1, 2, 3][..])
        );

        let reloaded = first_run.import(request()).expect("still loadable");
        assert!(reloaded.already_loaded);
        assert_eq!(reloaded.handle, created.handle);

        let mut second_run = AssetManager::new();
        let read = second_run.import(request()).expect("the metadata exists");
        assert_eq!(read.metadata, MetadataSource::ReadFromFile);
        assert_eq!(read.guid, created.guid);

        // The same file, by extension through the registry the module's
        // registration fills, without naming `Sound`.
        let mut world = pill_engine::World::new();
        world.register_imported_asset::<Sound>();
        let registry = world.get_resource::<pill_engine::ImportRegistry>().unwrap();
        let erased = registry
            .import(
                &mut second_run,
                std::path::Path::new("sounds/click.mp3"),
                MetadataPolicy::ReadIfPresent,
            )
            .expect("`.mp3` maps to Sound");
        assert_eq!(erased.type_name, "pill_audio::Sound");
        assert!(erased.previously_loaded);
        assert_eq!(erased.guid, created.guid);

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// An imported file whose extension is not an audio format is refused as
    /// a decode error naming the file.
    #[test]
    fn importing_an_unsupported_format_is_refused() {
        let error = Sound::import("sounds/notes.txt", &[1], &SoundImportSettings::default())
            .err()
            .expect("a text file is not a sound");
        assert!(
            matches!(error, AssetLoadError::Decode { ref label, .. } if label == "sounds/notes.txt")
        );
    }

    /// Bytes already in memory skip path validation, which is what an embedded
    /// or cooked asset needs.
    #[test]
    fn sound_can_be_built_from_bytes() {
        let sound = Sound::from_bytes(std::path::Path::new("embedded.wav"), vec![1, 2, 3]);
        assert_eq!(sound.bytes(), &[1, 2, 3]);
        assert_eq!(sound.path(), std::path::Path::new("embedded.wav"));
    }

    /// A sound is an asset, so it is stored many-per-type and reached by name.
    #[test]
    fn sounds_are_stored_as_assets() {
        let mut assets = AssetManager::new();
        let handle = assets
            .add_named(
                "footstep",
                Sound::from_bytes(std::path::Path::new("a.wav"), vec![7]),
            )
            .expect("a fresh name");

        assert_eq!(assets.get(handle).map(Sound::bytes), Some(&[7][..]));
        assert_eq!(
            assets.get_by_name::<Sound>("footstep").map(Sound::bytes),
            Some(&[7][..])
        );
        assert_eq!(assets.len::<Sound>(), 1);
    }

    /// Unloading a sound leaves an `AudioSourceComponent` naming it resolving to
    /// nothing, which is what replaces the old engine's `destroy` hook that
    /// reached into every component.
    #[test]
    fn unloading_a_sound_leaves_its_name_unresolvable() {
        let mut assets = AssetManager::new();
        let handle = assets
            .add_named(
                "footstep",
                Sound::from_bytes(std::path::Path::new("a.wav"), vec![7]),
            )
            .expect("a fresh name");
        assets.remove(handle);

        assert!(assets.get_by_name::<Sound>("footstep").is_none());
    }
}
