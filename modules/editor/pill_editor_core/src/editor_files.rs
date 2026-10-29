//! The editor's own state documents on disk.
//!
//! # Responsibilities
//!
//! - One durability rule for the editor's JSON state files: create the
//!   directory, write a temporary file beside the target and rename it into
//!   place, so a crash mid-write never leaves a half-written document.
//! - One error surface ([`FileError`]) for a read, a write, a parse and a
//!   removal, whatever document it is.
//!
//! # Design
//!
//! The documents differ - a dock layout, the preferences map, the recovery
//! snapshot, the open scene - so the helper is generic over `serde` and takes
//! a full path. What they share is the file handling, and that is what lives
//! here: three of the four call sites had their own `create_dir_all`, their own
//! temporary name and their own failure story.
//!
//! Two policies are stated once instead of per call site:
//!
//! - **A missing file is empty, not broken.** `read_json` answers `Ok(None)`
//!   only for `NotFound`. A document that exists and cannot be read or parsed
//!   is an error, never a blank slate: folding it into an empty one would let
//!   the next save write a file holding only the keys it set, erasing every
//!   unrelated preference.
//! - **A rename replaces.** `std::fs::rename` maps to `MoveFileExW` with
//!   `MOVEFILE_REPLACE_EXISTING` on Windows and to `rename(2)` elsewhere, so it
//!   replaces an existing target on every platform the editor ships on. The
//!   layout writer used to keep a `.bak` fallback for the opposite belief; the
//!   two-save tests in `preferences` and `layout::persistence` pin the fact.

// Standard library
use std::fmt;
use std::io;
use std::path::Path;

// External crates
use serde::de::DeserializeOwned;
use serde::Serialize;

/// The suffix of the file a write stages beside its target.
const TEMPORARY_SUFFIX: &str = "tmp";

/// What can go wrong with one of the editor's state files.
#[derive(Debug)]
pub enum FileError {
    /// The file could not be read. A missing file is not this: it is
    /// `Ok(None)`.
    Read {
        /// The file, as written in the message.
        path: String,
        /// What the filesystem said.
        source: io::Error,
    },
    /// The file holds text that is not the JSON the caller expects.
    Parse {
        /// The file, as written in the message.
        path: String,
        /// What the parser said.
        message: String,
    },
    /// The value could not be serialized.
    Serialize {
        /// The file, as written in the message.
        path: String,
        /// What the serializer said.
        message: String,
    },
    /// The file could not be written or replaced.
    Write {
        /// The file, as written in the message.
        path: String,
        /// What the filesystem said.
        source: io::Error,
    },
}

impl FileError {
    /// The one-line text the Console and the callers that still report a
    /// `String` show.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            FileError::Read { path, source } => format!("{path}: {source}"),
            FileError::Parse { path, message } => format!("{path} is not valid JSON: {message}"),
            FileError::Serialize { path, message } => {
                format!("cannot serialize `{path}`: {message}")
            }
            FileError::Write { path, source } => format!("cannot write `{path}`: {source}"),
        }
    }
}

impl fmt::Display for FileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message())
    }
}

impl std::error::Error for FileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FileError::Read { source, .. } | FileError::Write { source, .. } => Some(source),
            FileError::Parse { .. } | FileError::Serialize { .. } => None,
        }
    }
}

/// Read one JSON document; `None` when the file does not exist.
///
/// # Errors
///
/// A [`FileError::Read`] when the file exists and cannot be read, and a
/// [`FileError::Parse`] when its text is not the document the caller asked
/// for.
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, FileError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(FileError::Read {
                path: path.display().to_string(),
                source,
            })
        }
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|error| FileError::Parse {
            path: path.display().to_string(),
            message: error.to_string(),
        })
}

/// Serialize and write one JSON document.
///
/// # Errors
///
/// A [`FileError::Serialize`] when the value cannot be serialized and a
/// [`FileError::Write`] when the file cannot be replaced.
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), FileError> {
    let text = serde_json::to_string_pretty(value).map_err(|error| FileError::Serialize {
        path: path.display().to_string(),
        message: error.to_string(),
    })?;
    write_bytes(path, text.as_bytes())
}

/// Write one document's bytes through a temporary file and a rename.
///
/// # Errors
///
/// A [`FileError::Write`] naming the directory, the temporary file or the
/// target, whichever failed.
pub fn write_bytes(path: &Path, bytes: &[u8]) -> Result<(), FileError> {
    let failure = |target: &Path, source: io::Error| FileError::Write {
        path: target.display().to_string(),
        source,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| failure(parent, source))?;
    }
    let temporary = path.with_extension(TEMPORARY_SUFFIX);
    std::fs::write(&temporary, bytes).map_err(|source| failure(&temporary, source))?;
    std::fs::rename(&temporary, path).map_err(|source| {
        // A failed rename leaves the staged file behind; take it back so the
        // next write does not find it.
        let _ = std::fs::remove_file(&temporary);
        failure(path, source)
    })
}

/// Remove one document; a file that is already gone is not an error.
///
/// # Errors
///
/// A [`FileError::Write`] when the file exists and cannot be removed.
pub fn remove(path: &Path) -> Result<(), FileError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(FileError::Write {
            path: path.display().to_string(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory of this test's own under the system temp directory,
    /// named so parallel runs do not collide.
    fn scratch_directory(label: &str) -> std::path::PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "ecs-editor-files-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&directory).expect("the scratch directory");
        directory
    }

    /// A document that does not exist reads as absent, and the write creates
    /// the directory it belongs in.
    #[test]
    fn a_missing_document_is_absent_and_a_write_creates_its_directory() {
        let directory = scratch_directory("missing");
        let path = directory.join("nested").join("state.json");
        assert!(read_json::<serde_json::Value>(&path)
            .expect("an absent document is not an error")
            .is_none());
        write_json(&path, &serde_json::json!({ "kept": true })).expect("the write");
        let read: serde_json::Value = read_json(&path).expect("the read").expect("the document");
        assert_eq!(read, serde_json::json!({ "kept": true }));
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// A document that exists and cannot be parsed is an error, not an empty
    /// one: the next save must not erase it.
    #[test]
    fn an_unparsable_document_is_an_error() {
        let directory = scratch_directory("unparsable");
        let path = directory.join("state.json");
        std::fs::write(&path, "{ \"a\": 1, }").expect("the broken document");
        let error = read_json::<serde_json::Value>(&path).expect_err("a broken document errors");
        assert!(
            matches!(error, FileError::Parse { .. }),
            "{error:?} is not a parse error"
        );
        assert!(error.message().contains("is not valid JSON"), "{error}");
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// Writing the same document twice replaces it: the rename is a replace on
    /// every platform the editor ships on, which is what the layout writer's
    /// removed `.bak` fallback doubted.
    #[test]
    fn the_second_write_replaces_the_first() {
        let directory = scratch_directory("twice");
        let path = directory.join("state.json");
        write_json(&path, &serde_json::json!({ "generation": 1 })).expect("the first write");
        write_json(&path, &serde_json::json!({ "generation": 2 })).expect("the second write");
        let read: serde_json::Value = read_json(&path).expect("the read").expect("the document");
        assert_eq!(read, serde_json::json!({ "generation": 2 }));
        let staged = path.with_extension(TEMPORARY_SUFFIX);
        assert!(!staged.exists(), "the staged file was left behind");
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// Removing a document that is not there is not an error.
    #[test]
    fn removing_an_absent_document_is_not_an_error() {
        let directory = scratch_directory("remove");
        let path = directory.join("state.json");
        remove(&path).expect("an absent document is not an error");
        write_json(&path, &serde_json::json!({})).expect("the write");
        remove(&path).expect("the removal");
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&directory);
    }
}
