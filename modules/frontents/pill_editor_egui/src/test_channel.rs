//! The editor's scripted test channel: the file the devops suites append
//! commands to (Stage 8).
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
//! Commands are handled by the runner itself (quit, dock rearrangement), so
//! the channel stays free of window or engine types.

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
    /// A file that shrinks (the suite truncated it) restarts from the top so
    /// the truncated run's commands are not skipped.
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
        let appended = content[self.consumed..].to_string();
        self.consumed = content.len();
        appended
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
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
