//! Polling the editor's state files for changes.
//!
//! # Responsibilities
//!
//! - Notice that one of the editor's state files changed without a writer to
//!   hook: the editor's files are also written by hand and by other tools, so
//!   the file's own metadata is the signal ([`FileStamp`]).
//! - Read that file at most once per change, and keep its text so an unchanged
//!   file costs nothing ([`StateFileWatch`]).
//!
//! # Design
//!
//! A watcher re-reads when the stamp moved or when the grace poll is due, so a
//! coarse or unchanged timestamp still reaches the editor within a fraction of
//! a second. The same rule was written out twice in the executable, with one
//! extra flag between them; the flag's meaning is now the caller's decision:
//! a caller whose apply can fail calls [`StateFileWatch::poll_now`] until it
//! has applied once, instead of the watch carrying a "have I been applied"
//! bit.
//!
//! The theme's layers keep their own stamps: they name a *selection* to
//! re-resolve rather than a document to read, so a text cache would be dead
//! weight there.

// Standard library
use std::path::{Path, PathBuf};

/// How often a state file is re-read even when its stamp did not move.
///
/// A filesystem may report a modification time coarser than the edits to the
/// file, so the stamp alone can miss a change; this poll bounds the wait.
pub const STATE_FILE_POLL_FRAMES: u64 = 30;

/// A state file's metadata: what a frame compares to decide whether the file
/// needs reading at all.
///
/// A missing file stamps as the default, which differs from any file that
/// exists, so creating one is noticed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileStamp {
    /// The file's modification time, when the filesystem reports one.
    modified: Option<std::time::SystemTime>,
    /// Its length in bytes.
    length: u64,
}

impl FileStamp {
    /// The stamp of `path`, or the default when it cannot be read.
    #[must_use]
    pub fn of(path: &Path) -> Self {
        std::fs::metadata(path)
            .map(|metadata| Self {
                modified: metadata.modified().ok(),
                length: metadata.len(),
            })
            .unwrap_or_default()
    }
}

/// One state file the editor polls: where it is, the stamp its text was read
/// at, and the text that read produced.
#[derive(Debug, Default)]
pub struct StateFileWatch {
    path: PathBuf,
    stamp: FileStamp,
    text: Option<String>,
    read_once: bool,
}

impl StateFileWatch {
    /// A watch on `path` that has not read it yet.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            stamp: FileStamp::default(),
            text: None,
            read_once: false,
        }
    }

    /// The path this watch polls.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The text of the last read; `None` when the file was absent.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }

    /// Re-read the file when its stamp moved or the grace poll is due.
    ///
    /// `None` means the file is unchanged; `Some(None)` means it is gone;
    /// `Some(Some(text))` is the new content. The first poll always reads,
    /// because the default stamp of a file that has never been read differs
    /// from the stamp of any file that exists.
    pub fn poll(&mut self, frame_index: u64) -> Option<Option<String>> {
        let stamp = FileStamp::of(&self.path);
        let due = frame_index.is_multiple_of(STATE_FILE_POLL_FRAMES);
        if !due && self.read_once && stamp == self.stamp {
            return None;
        }
        self.stamp = stamp;
        self.read()
    }

    /// Re-read the file whatever its stamp says, answering only when the text
    /// changed.
    ///
    /// A caller whose apply can fail polls this way until it has applied once,
    /// so a file that could not be applied is retried instead of being
    /// silently skipped as unchanged.
    pub fn poll_now(&mut self) -> Option<Option<String>> {
        self.stamp = FileStamp::of(&self.path);
        self.read()
    }

    /// Read the file and answer its text when it differs from the cached one,
    /// or when this is its first read.
    fn read(&mut self) -> Option<Option<String>> {
        let text = std::fs::read_to_string(&self.path).ok();
        let first = !self.read_once;
        self.read_once = true;
        if !first && text == self.text {
            return None;
        }
        self.text = text;
        Some(self.text.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory of this test's own under the system temp directory,
    /// named so parallel runs do not collide.
    fn scratch_directory(label: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "ecs-state-watch-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&directory).expect("the scratch directory");
        directory
    }

    /// The four states a watcher has to tell apart: absent, present, unchanged
    /// and edited.
    #[test]
    fn a_watch_follows_absent_present_unchanged_and_edited() {
        let directory = scratch_directory("follow");
        let path = directory.join("state.json");
        let mut watch = StateFileWatch::new(path.clone());

        // Absent: the first poll reads and answers the absence.
        assert_eq!(watch.poll(1), Some(None), "the first poll reads");
        assert_eq!(watch.text(), None);
        // Still absent: unchanged.
        assert_eq!(watch.poll(2), None);

        // Present: the stamp moved, so the poll reads the file.
        std::fs::write(&path, "{\"a\": 1}").expect("the file");
        assert_eq!(watch.poll(3), Some(Some("{\"a\": 1}".to_owned())));
        assert_eq!(watch.text(), Some("{\"a\": 1}"));

        // Unchanged: no stamp move, no read.
        assert_eq!(watch.poll(4), None);

        // Edited: the stamp moved again.
        std::fs::write(&path, "{\"a\": 2}").expect("the edit");
        assert_eq!(watch.poll(5), Some(Some("{\"a\": 2}".to_owned())));

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// The grace poll re-reads even when the stamp did not move, which is what
    /// catches a filesystem with a coarse modification time.
    #[test]
    fn the_grace_poll_reads_an_unchanged_stamp() {
        let directory = scratch_directory("grace");
        let path = directory.join("state.json");
        std::fs::write(&path, "{\"a\": 1}").expect("the file");
        let mut watch = StateFileWatch::new(path.clone());
        assert_eq!(watch.poll(1), Some(Some("{\"a\": 1}".to_owned())));
        // A poll at the grace frame re-reads even though the stamp compares
        // equal; the text is the same, so it answers nothing.
        assert_eq!(watch.poll(STATE_FILE_POLL_FRAMES), None);
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// A caller that cannot apply yet asks for a re-read regardless of the
    /// stamp, and gets one only when the text really changed.
    #[test]
    fn poll_now_reads_without_a_stamp_move() {
        let directory = scratch_directory("now");
        let path = directory.join("state.json");
        std::fs::write(&path, "{\"a\": 1}").expect("the file");
        let mut watch = StateFileWatch::new(path.clone());
        assert_eq!(watch.poll(1), Some(Some("{\"a\": 1}".to_owned())));
        assert_eq!(watch.poll_now(), None, "the same text is not a change");
        std::fs::write(&path, "{\"a\": 2}").expect("the edit");
        assert_eq!(watch.poll_now(), Some(Some("{\"a\": 2}".to_owned())));
        let _ = std::fs::remove_dir_all(&directory);
    }
}
