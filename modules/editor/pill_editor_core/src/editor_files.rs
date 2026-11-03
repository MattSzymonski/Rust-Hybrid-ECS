//! Reads, writes and removes the editor's own state documents on disk.
//!
//! # Responsibilities
//!
//! - One durability rule for the editor's JSON state files. Create the
//!   directory, write a temporary file beside the target, then rename it into
//!   place, so a crash mid-write never leaves a half-written document.
//! - One error type ([`FileError`]) for every read, write, parse and removal,
//!   whatever document it belongs to.
//!
//! # Design
//!
//! The documents differ - a dock layout, the preferences map, the recovery
//! snapshot, the open scene - so the JSON helpers are generic over `serde`
//! and every function takes a full path. What the documents share is the file
//! handling, and that is what this module holds.
//!
//! Two policies are stated once instead of per call site:
//!
//! - **A missing file is empty, not broken.** [`read_json`] answers `Ok(None)`
//!   only for `NotFound`. A document that exists and cannot be read or parsed
//!   is an error, never a blank slate. Folding it into an empty one would let
//!   the next save write a file holding only the keys it set, erasing every
//!   unrelated preference.
//! - **A rename replaces.** `std::fs::rename` maps to `MoveFileExW` with
//!   `MOVEFILE_REPLACE_EXISTING` on Windows and to `rename(2)` elsewhere, so it
//!   replaces an existing target on every platform the editor ships on. The
//!   two-save tests in `preferences` and `layout::persistence` check it.

// Standard library
use std::fmt;
use std::io;
use std::path::Path;

// External crates
use serde::de::DeserializeOwned;
use serde::Serialize;

// =============================================================================
// Constants
// =============================================================================

/// The extension of the temporary file a write creates beside its target.
const TEMPORARY_FILE_EXTENSION: &str = "tmp";

// =============================================================================
// FileError
// =============================================================================

/// What can go wrong with one of the editor's state files.
#[derive(Debug)]
pub enum FileError {
    /// The file could not be read. A missing file is reported as `Ok(None)`,
    /// not as this.
    Read {
        /// The file's path, spelled the way the messages show it.
        path: String,
        /// What the filesystem said.
        source: io::Error,
    },
    /// The file holds text that is not the JSON the caller expects.
    Parse {
        /// The file's path, spelled the way the messages show it.
        path: String,
        /// What the parser said.
        message: String,
    },
    /// The document could not be serialized.
    Serialize {
        /// The file's path, spelled the way the messages show it.
        path: String,
        /// What the serializer said.
        message: String,
    },
    /// The file could not be written or replaced.
    Write {
        /// The file's path, spelled the way the messages show it.
        path: String,
        /// What the filesystem said.
        source: io::Error,
    },
}

impl FileError {
    /// The one-line text for this error. `Display` prints it, and the Console
    /// tab and the callers that report a `String` show it.
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

// =============================================================================
// Free Functions
// =============================================================================

/// Reads one JSON document, or `None` when the file does not exist.
///
/// # Errors
///
/// Returns [`FileError::Read`] when the file exists and cannot be read, and
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

/// Serializes and writes one JSON document.
///
/// # Errors
///
/// Returns [`FileError::Serialize`] when the document cannot be serialized and
/// [`FileError::Write`] when the file cannot be replaced.
pub fn write_json<T: Serialize>(path: &Path, document: &T) -> Result<(), FileError> {
    let text = serde_json::to_string_pretty(document).map_err(|error| FileError::Serialize {
        path: path.display().to_string(),
        message: error.to_string(),
    })?;
    write_bytes(path, text.as_bytes())
}

/// Writes one document's bytes through a temporary file and a rename.
///
/// # Errors
///
/// Returns [`FileError::Write`] naming the directory, the temporary file or
/// the target, whichever failed.
pub fn write_bytes(path: &Path, bytes: &[u8]) -> Result<(), FileError> {
    let write_error = |target: &Path, source: io::Error| FileError::Write {
        path: target.display().to_string(),
        source,
    };
    // Step 1: Make the directory the document belongs in.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| write_error(parent, source))?;
    }
    // Step 2: Write the whole document into a temporary file beside the target.
    let temporary_file = path.with_extension(TEMPORARY_FILE_EXTENSION);
    std::fs::write(&temporary_file, bytes)
        .map_err(|source| write_error(&temporary_file, source))?;
    // Step 3: Rename the temporary file over the target, replacing what is there.
    std::fs::rename(&temporary_file, path).map_err(|source| {
        // A failed rename leaves the temporary file behind. Remove it so the
        // next write does not find it.
        let _ = std::fs::remove_file(&temporary_file);
        write_error(path, source)
    })
}

/// Removes one document. A file that is already gone is not an error.
///
/// # Errors
///
/// Returns [`FileError::Write`] when the file exists and cannot be removed.
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

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates a scratch directory of this test's own under the system temp
    /// directory. Its name carries the process id and a nanosecond timestamp,
    /// so parallel runs do not collide.
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
    /// one. Otherwise the next save would erase it.
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

    /// Writing the same document twice replaces it. The rename replaces an
    /// existing target on every platform the editor ships on.
    #[test]
    fn the_second_write_replaces_the_first() {
        let directory = scratch_directory("twice");
        let path = directory.join("state.json");
        write_json(&path, &serde_json::json!({ "generation": 1 })).expect("the first write");
        write_json(&path, &serde_json::json!({ "generation": 2 })).expect("the second write");
        let read: serde_json::Value = read_json(&path).expect("the read").expect("the document");
        assert_eq!(read, serde_json::json!({ "generation": 2 }));
        let staged_file = path.with_extension(TEMPORARY_FILE_EXTENSION);
        assert!(!staged_file.exists(), "the staged file was left behind");
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
