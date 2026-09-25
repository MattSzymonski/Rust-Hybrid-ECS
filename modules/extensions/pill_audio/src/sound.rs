//! The [`Sound`] asset: an audio file held as its undecoded bytes.
//!
//! # Responsibilities
//!
//! - Declares [`Sound`], stored in the
//!   [`AssetManager`](pill_engine::AssetManager) and addressed by
//!   `Handle<Sound>`.
//! - Validates and reads an audio file from disk ([`Sound::load`]).
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
use pill_engine::Asset;

// =============================================================================
// Constants
// =============================================================================

/// Audio container formats [`Sound::load`] accepts.
///
/// Checked by `pill_core::utils::validate_asset_path` before any bytes are
/// read, so a typo in a path fails with the path and the allowed list rather
/// than as a decoder error thousands of samples later.
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

/// Why [`Sound::load`] could not produce a sound.
#[derive(Debug)]
pub enum SoundLoadError {
    /// The path does not exist, or its extension is not a supported format.
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

impl Sound {
    /// Read an audio file into memory.
    ///
    /// The path is validated against [`SUPPORTED_AUDIO_FORMATS`] before it is
    /// opened, so an unsupported extension is reported as such rather than as
    /// a decode failure.
    ///
    /// # Errors
    ///
    /// [`SoundLoadError::InvalidPath`] when the path does not exist or its
    /// format is not supported, and [`SoundLoadError::Unreadable`] when the
    /// file cannot be read.
    pub fn load(path: &std::path::Path) -> Result<Self, SoundLoadError> {
        let path = path.to_path_buf();
        pill_core::utils::validate_asset_path(&path, SUPPORTED_AUDIO_FORMATS)
            .map_err(SoundLoadError::InvalidPath)?;

        let bytes = std::fs::read(&path).map_err(|source| SoundLoadError::Unreadable {
            path: path.clone(),
            source,
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
    use pill_engine::AssetManager;

    /// A missing file is reported as an invalid path, before any read.
    #[test]
    fn loading_a_missing_file_fails_with_the_path() {
        let error = Sound::load(std::path::Path::new("definitely/not/here.wav")).unwrap_err();
        assert!(matches!(error, SoundLoadError::InvalidPath(_)));
    }

    /// An unsupported extension is rejected as a format problem rather than
    /// surfacing later as a decode failure.
    ///
    /// Needs a file that genuinely exists, or the path check fails first and
    /// the format is never reached.
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
