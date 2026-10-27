//! The editor's scripted test channel: the file the devops suites append
//! commands to.
//!
//! # Responsibilities
//!
//! - [`TestChannel`]: tail the file named by `PILL_EDITOR_TEST_COMMANDS`,
//!   returning only the lines appended since the last poll.
//! - [`seconds_from_environment`]: the `PILL_EDITOR_TEST_SECONDS` auto-quit
//!   delay, so a wedged run still ends.
//!
//! # Design
//!
//! The channel is deliberately read-only and line-based: the suite appends
//! one command per line, and the runner executes the lines it has not seen.
//! Only complete lines are handed over - a line whose newline has not arrived
//! yet waits for the next poll, so a write that lands in two pieces never runs
//! half a verb - and a `#` line is a comment the channel skips, so a suite can
//! annotate its command file. Commands are handled by the runner itself (quit,
//! dock rearrangement), so the channel stays free of window or engine types.

// Standard library
use std::path::PathBuf;

// =============================================================================
// TestChannel
// =============================================================================

/// The scripted command channel: a file whose appended lines are commands.
pub struct TestChannel {
    /// The file the suites append to.
    path: PathBuf,
    /// How many bytes of the file have been handled.
    consumed: usize,
}

impl TestChannel {
    /// Open the channel when `PILL_EDITOR_TEST_COMMANDS` names a file.
    pub fn from_environment() -> Option<TestChannel> {
        let path = std::env::var_os("PILL_EDITOR_TEST_COMMANDS")?;
        Some(TestChannel {
            path: PathBuf::from(path),
            consumed: 0,
        })
    }

    /// The command lines appended since the last poll.
    ///
    /// Only complete lines run: a trailing fragment without its newline stays
    /// unconsumed until the writer finishes it, so a suite whose write lands
    /// in two pieces cannot have the first piece run as a verb of its own. A
    /// file that shrinks (the suite truncated it) restarts from the top so the
    /// truncated run's commands are not skipped. Blank lines and `#` comments
    /// are not commands and never reach the runner.
    pub fn poll(&mut self) -> Vec<String> {
        let content = match std::fs::read_to_string(&self.path) {
            Ok(content) => content,
            Err(_) => return Vec::new(),
        };
        if content.len() < self.consumed {
            self.consumed = 0;
        }
        if content.len() == self.consumed {
            return Vec::new();
        }
        // The last newline ends the last complete line; anything after it is a
        // write still in progress and stays for the next poll.
        let Some(last_newline) = content[self.consumed..].rfind('\n') else {
            return Vec::new();
        };
        let boundary = self.consumed + last_newline + 1;
        let ready = &content[self.consumed..boundary];
        self.consumed = boundary;
        ready
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(String::from)
            .collect()
    }
}

// =============================================================================
// Auto-quit
// =============================================================================

/// How many seconds the editor runs before quitting gracefully, when
/// `PILL_EDITOR_TEST_SECONDS` asks for one.
pub fn seconds_from_environment() -> Option<f64> {
    std::env::var("PILL_EDITOR_TEST_SECONDS")
        .ok()
        .and_then(|value| value.trim().parse().ok())
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::TestChannel;

    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A per-test file name: the tests run in parallel inside one process.
    static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

    fn temp_file(prefix: &str) -> PathBuf {
        let index = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "pill_test_channel_{prefix}_{}_{index}.txt",
            std::process::id()
        ))
    }

    fn open(path: &Path) -> TestChannel {
        TestChannel {
            path: path.to_path_buf(),
            consumed: 0,
        }
    }

    fn append(path: &Path, text: &str) {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("the test channel file opens");
        file.write_all(text.as_bytes())
            .expect("the test channel write succeeds");
    }

    #[test]
    fn a_partial_line_waits_for_its_newline() {
        let path = temp_file("partial");
        let mut channel = open(&path);
        append(&path, "clic");
        assert!(
            channel.poll().is_empty(),
            "a half-written line is not a verb"
        );
        append(&path, "k 10 20\n");
        assert_eq!(channel.poll(), vec!["click 10 20".to_string()]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn comments_and_blank_lines_are_not_commands() {
        let path = temp_file("comments");
        let mut channel = open(&path);
        append(&path, "# the scene step\n\n   \nclick 1 2\n");
        assert_eq!(channel.poll(), vec!["click 1 2".to_string()]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn only_appended_lines_run() {
        let path = temp_file("appended");
        let mut channel = open(&path);
        append(&path, "quit\n");
        assert_eq!(channel.poll(), vec!["quit".to_string()]);
        assert!(channel.poll().is_empty(), "a handled line runs once");
        append(&path, "tab scene\n");
        assert_eq!(channel.poll(), vec!["tab scene".to_string()]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_truncated_file_restarts_from_the_top() {
        let path = temp_file("truncated");
        let mut channel = open(&path);
        append(&path, "click 1 2\nquit\n");
        assert_eq!(channel.poll().len(), 2);
        std::fs::write(&path, "tab scene\n").expect("the test channel file rewrites");
        assert_eq!(channel.poll(), vec!["tab scene".to_string()]);
        let _ = std::fs::remove_file(&path);
    }
}
