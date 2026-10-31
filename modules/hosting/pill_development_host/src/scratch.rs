//! Test-only facilities: scratch directories and explicit file dates.
//!
//! # Responsibilities
//!
//! - Hand every host test a fresh directory under the process's temporary
//!   root, named for the test plus the process id and a counter, so two test
//!   processes - a second checkout, a second agent - never share a path, and
//!   remove it on drop, which a failing test still reaches.
//! - Date a file explicitly, for tests whose subject is "this file is newer
//!   than that one", so the comparison rests on the date rather than on
//!   filesystem timestamp resolution and the wall clock.
//!
//! # Design
//!
//! One guard for the crate means one naming scheme, and cleanup in `Drop`
//! rather than in a trailing call, so a failing test - which never reaches the
//! end of its body - still removes its directory.

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

/// Distinguishes directories made by the same test in one process.
static NEXT_SCRATCH: AtomicUsize = AtomicUsize::new(0);

/// A fresh directory under the process's temporary root, removed on drop.
pub(crate) struct ScratchDirectory {
    /// The directory this guard owns.
    path: PathBuf,
}

impl ScratchDirectory {
    /// Create an empty directory named `pill_host_<name>_<pid>_<counter>`.
    ///
    /// `name` is the test's own label, so a directory found in the temporary
    /// root names the test that leaked it.
    pub(crate) fn new(name: &str) -> Self {
        let counter = NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("pill_host_{name}_{}_{counter}", std::process::id()));
        // A same-named directory can only be one of ours from an earlier run
        // of the same process id, so it is safe to clear.
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create the scratch directory");
        Self { path }
    }

    /// The directory itself, for tests that hand it to the code under test.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Write a file inside the directory, creating parent directories, and
    /// return its path.
    pub(crate) fn write(&self, relative: &str, contents: impl AsRef<[u8]>) -> PathBuf {
        let path = self.path.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create the scratch parent directory");
        }
        std::fs::write(&path, contents).expect("write the scratch file");
        path
    }
}

impl Deref for ScratchDirectory {
    type Target = Path;

    /// The directory, so `root.join("x")` and `&root` at a `&Path` parameter
    /// both work on the guard directly.
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for ScratchDirectory {
    /// The directory, for the generic `AsRef<Path>` parameters of `std::fs`.
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchDirectory {
    fn drop(&mut self) {
        // A directory the test already removed is fine; cleanup is
        // best-effort by design.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Set a file's modification time, so a later write is provably newer.
///
/// Dating one side explicitly makes a "this file is newer than that one"
/// comparison true by construction, with no wall-clock distance to wait out.
/// Files only: Windows cannot open a directory to set its time, so a fixture
/// whose walk compares directory times must stay flat.
pub(crate) fn date_file(path: &Path, modified: SystemTime) {
    let file = std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap_or_else(|error| panic!("open {} to date it: {error}", path.display()));
    file.set_modified(modified)
        .unwrap_or_else(|error| panic!("set the modification time of {}: {error}", path.display()));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two guards in one process never share a path, and both exist.
    #[test]
    fn guards_get_distinct_live_directories() {
        let first = ScratchDirectory::new("distinct");
        let second = ScratchDirectory::new("distinct");
        assert_ne!(first.path(), second.path());
        assert!(first.path().is_dir());
        assert!(second.path().is_dir());
    }

    /// Dropping the guard removes the directory, files and all.
    #[test]
    fn drop_removes_the_directory() {
        let path = {
            let scratch = ScratchDirectory::new("cleanup");
            scratch.write("nested/file.txt", "contents");
            assert!(scratch.join("nested/file.txt").is_file());
            scratch.path().to_path_buf()
        };
        assert!(
            !path.exists(),
            "the guard must remove its directory on drop"
        );
    }
}
