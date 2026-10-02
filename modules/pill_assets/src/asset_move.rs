//! Moving a source asset together with its metadata file.
//!
//! # Responsibilities
//!
//! - Move a source file and its `<file>.meta` sidecar as one operation
//!   ([`move_asset`]), so the guid the metadata holds follows the file.
//! - Refuse a move that would overwrite anything, or that names no source.
//!
//! # Design
//!
//! A source moved without its `.meta` loses its guid: the next import writes a
//! fresh one beside it, and every saved reference to the old guid stops
//! resolving. This is the one supported way to rename an asset; tools (the
//! editor, scripts) call it instead of moving files themselves.
//!
//! The source moves first and the metadata second. When the second rename
//! fails, the first is undone, so the pair is never left split on purpose. A
//! crash between the two renames can still split it; the dev host's watcher
//! and the `test_asset_metadata.py` check are what notice that.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

/// The suffix a metadata file adds to its source's full file name.
const METADATA_SUFFIX: &str = ".meta";

/// Why [`move_asset`] did not move anything.
#[derive(Debug)]
pub enum MoveAssetError {
    /// There is no source file at the origin.
    SourceMissing {
        /// The origin that was asked for.
        path: PathBuf,
    },
    /// The origin is itself a metadata file; move its source instead.
    IsMetadataFile {
        /// The origin that was asked for.
        path: PathBuf,
    },
    /// A file already exists where the source or its metadata would go.
    TargetExists {
        /// The file that would have been overwritten.
        path: PathBuf,
    },
    /// A filesystem operation failed; nothing was left half-moved.
    Io {
        /// The path the operation touched.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
}

impl fmt::Display for MoveAssetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourceMissing { path } => {
                write!(
                    formatter,
                    "there is no asset at `{}` to move",
                    path.display()
                )
            }
            Self::IsMetadataFile { path } => write!(
                formatter,
                "`{}` is a metadata file; move its source and it follows",
                path.display()
            ),
            Self::TargetExists { path } => write!(
                formatter,
                "`{}` already exists; an asset move never overwrites",
                path.display()
            ),
            Self::Io { path, source } => {
                write!(formatter, "could not move `{}`: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for MoveAssetError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// The metadata file of the source at `path`: its full name with `.meta`
/// appended.
pub fn metadata_path_of(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(METADATA_SUFFIX);
    PathBuf::from(name)
}

/// Move the source asset at `from` to `to`, together with its `.meta` file
/// when it has one, creating `to`'s directory if needed.
///
/// Both paths are filesystem paths. Nothing is overwritten: the move is
/// refused when a file exists at `to` or at `to`'s metadata path.
///
/// # Errors
///
/// [`MoveAssetError::SourceMissing`] when `from` is not a file,
/// [`MoveAssetError::IsMetadataFile`] when it is a `.meta`,
/// [`MoveAssetError::TargetExists`] when either target exists, and
/// [`MoveAssetError::Io`] when a rename fails; a failed metadata rename moves
/// the source back first.
pub fn move_asset(from: &Path, to: &Path) -> Result<(), MoveAssetError> {
    if from
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("meta"))
    {
        return Err(MoveAssetError::IsMetadataFile {
            path: from.to_owned(),
        });
    }
    if !from.is_file() {
        return Err(MoveAssetError::SourceMissing {
            path: from.to_owned(),
        });
    }
    let from_metadata = metadata_path_of(from);
    let to_metadata = metadata_path_of(to);
    // Both targets are checked before anything moves, so a refusal leaves the
    // origin exactly as it was.
    for target in [to, to_metadata.as_path()] {
        if target.exists() {
            return Err(MoveAssetError::TargetExists {
                path: target.to_owned(),
            });
        }
    }
    if let Some(directory) = to.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        fs::create_dir_all(directory).map_err(|source| MoveAssetError::Io {
            path: directory.to_owned(),
            source,
        })?;
    }

    fs::rename(from, to).map_err(|source| MoveAssetError::Io {
        path: from.to_owned(),
        source,
    })?;
    if from_metadata.is_file() {
        if let Err(source) = fs::rename(&from_metadata, &to_metadata) {
            // Undo the source move rather than leave the guid behind.
            let _ = fs::rename(to, from);
            return Err(MoveAssetError::Io {
                path: from_metadata,
                source,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory, removed afterwards.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("pill-asset-move-{label}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn write(&self, relative: &str, contents: &str) -> PathBuf {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, contents).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_source_and_its_metadata_move_together() {
        let scratch = Scratch::new("together");
        let from = scratch.write("a.png", "pixels");
        scratch.write("a.png.meta", "guid");
        let to = scratch.0.join("textures/b.png");

        move_asset(&from, &to).unwrap();

        assert_eq!(fs::read_to_string(&to).unwrap(), "pixels");
        assert_eq!(
            fs::read_to_string(scratch.0.join("textures/b.png.meta")).unwrap(),
            "guid"
        );
        assert!(!from.exists() && !scratch.0.join("a.png.meta").exists());
    }

    #[test]
    fn a_source_without_metadata_moves_alone() {
        let scratch = Scratch::new("alone");
        let from = scratch.write("a.png", "pixels");
        let to = scratch.0.join("b.png");

        move_asset(&from, &to).unwrap();

        assert!(to.is_file());
        assert!(!scratch.0.join("b.png.meta").exists());
    }

    #[test]
    fn an_existing_target_or_target_metadata_is_refused() {
        let scratch = Scratch::new("refused");
        let from = scratch.write("a.png", "pixels");
        scratch.write("a.png.meta", "guid");
        let taken = scratch.write("b.png", "other");
        scratch.write("c.png.meta", "stray");

        for target in [taken.clone(), scratch.0.join("c.png")] {
            let error = move_asset(&from, &target).unwrap_err();
            assert!(
                matches!(error, MoveAssetError::TargetExists { .. }),
                "{error}"
            );
        }
        // Nothing moved and nothing was overwritten.
        assert_eq!(fs::read_to_string(&from).unwrap(), "pixels");
        assert_eq!(fs::read_to_string(&taken).unwrap(), "other");
        assert!(scratch.0.join("a.png.meta").is_file());
    }

    #[test]
    fn a_missing_source_or_a_metadata_file_is_refused() {
        let scratch = Scratch::new("bad-origin");
        let metadata = scratch.write("a.png.meta", "guid");

        assert!(matches!(
            move_asset(&scratch.0.join("missing.png"), &scratch.0.join("x.png")),
            Err(MoveAssetError::SourceMissing { .. })
        ));
        assert!(matches!(
            move_asset(&metadata, &scratch.0.join("x.png.meta")),
            Err(MoveAssetError::IsMetadataFile { .. })
        ));
    }
}
