//! The end-to-end test channel: scripted input for the editor's devops
//! suite (plan decision D26, level 3).
//!
//! # Responsibilities
//!
//! - Reads a command file (path in `PILL_EDITOR_TEST_COMMANDS`) that the
//!   suite appends to while the editor runs.
//! - Parses each new line into a [`TestCommand`]: a synthetic click, a key
//!   with a delivery target, a chord (modifiers plus key), a command id to
//!   execute, a tab selection, or a quit request.
//!
//! # Design
//!
//! The file is re-read whole on every poll and lines already consumed are
//! skipped by count, so an append never races a read. The channel is inert
//! unless the environment variable names a file, which keeps a normal run
//! untouched; the executable is a development build, so nothing here is
//! compiled into anything shipped. The suite asserts on the `[editor]`
//! log lines the executable emits when it executes a command, which is also
//! how the input routing decision is observed.

// Standard library
use std::env;
use std::fs;
use std::path::PathBuf;

/// Environment variable naming the command file, appended to by the suite.
const COMMANDS_ENVIRONMENT_VARIABLE: &str = "PILL_EDITOR_TEST_COMMANDS";

/// Environment variable that requests a graceful quit after that many
/// seconds, for the exit-code checks.
pub const SECONDS_ENVIRONMENT_VARIABLE: &str = "PILL_EDITOR_TEST_SECONDS";

// =============================================================================
// TestCommand
// =============================================================================

/// One scripted command from the suite.
#[derive(Clone, Debug, PartialEq)]
pub enum TestCommand {
    /// Click at a point in logical client-area pixels.
    Click {
        /// Distance from the client area's left edge.
        x: f32,
        /// Distance from the client area's top edge.
        y: f32,
    },
    /// Press and release a named key (`escape`, `space`, or one letter),
    /// optionally with a delivery target: `ui` (the default), `scene` (the
    /// editor camera) or `game` (the project).
    Key {
        /// The key's lowercase name, as written in the command file.
        key: String,
        /// Where the key goes; `None` means the UI.
        target: Option<String>,
    },
    /// Select the tab whose title matches, by the shell's selection rule.
    Tab {
        /// The tab title to select.
        title: String,
    },
    /// Execute a registered command by id, through the shell's own dispatch.
    Command {
        /// The command id, as registered (`editor.pause_toggle`).
        id: String,
    },
    /// Press a chord in the `ctrl+shift+p` form, modifiers included.
    Chord {
        /// The chord text.
        text: String,
    },
    /// Quit gracefully, so the process exit code is the clean-shutdown one.
    Quit,
    /// A line that did not parse; logged and skipped by the caller.
    Unknown {
        /// The offending line.
        line: String,
    },
}

// =============================================================================
// TestChannel
// =============================================================================

/// The command file reader.
pub struct TestChannel {
    /// The file the suite appends commands to.
    path: PathBuf,
    /// How many lines have already been executed.
    consumed: usize,
}

impl TestChannel {
    /// The channel, when `PILL_EDITOR_TEST_COMMANDS` names a readable file.
    pub fn from_environment() -> Option<TestChannel> {
        let path = PathBuf::from(env::var_os(COMMANDS_ENVIRONMENT_VARIABLE)?);
        // A missing file is the suite's job to create; refuse quietly but
        // visibly rather than failing the editor's startup.
        if !path.is_file() {
            return None;
        }
        Some(TestChannel { path, consumed: 0 })
    }

    /// The commands appended since the last poll, in order.
    pub fn poll(&mut self) -> Vec<TestCommand> {
        let Ok(text) = fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        let mut commands = Vec::new();
        for line in text.lines().skip(self.consumed) {
            self.consumed += 1;
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            commands.push(parse_command(trimmed));
        }
        commands
    }
}

/// Parse one command line.
fn parse_command(line: &str) -> TestCommand {
    let mut parts = line.split_whitespace();
    match parts.next() {
        Some("click") => {
            let point = (|| {
                let x: f32 = parts.next()?.parse().ok()?;
                let y: f32 = parts.next()?.parse().ok()?;
                Some((x, y))
            })();
            match point {
                Some((x, y)) => TestCommand::Click { x, y },
                None => TestCommand::Unknown {
                    line: line.to_string(),
                },
            }
        }
        Some("key") => match parts.next() {
            Some(key) => TestCommand::Key {
                key: key.to_lowercase(),
                target: parts.next().map(str::to_lowercase),
            },
            None => TestCommand::Unknown {
                line: line.to_string(),
            },
        },
        Some("tab") => match parts.next() {
            Some(title) => TestCommand::Tab {
                title: title.to_string(),
            },
            None => TestCommand::Unknown {
                line: line.to_string(),
            },
        },
        Some("command") => match parts.next() {
            Some(id) => TestCommand::Command { id: id.to_string() },
            None => TestCommand::Unknown {
                line: line.to_string(),
            },
        },
        Some("chord") => match parts.next() {
            Some(text) => TestCommand::Chord {
                text: text.to_string(),
            },
            None => TestCommand::Unknown {
                line: line.to_string(),
            },
        },
        Some("quit") => TestCommand::Quit,
        _ => TestCommand::Unknown {
            line: line.to_string(),
        },
    }
}

/// The seconds the environment asked the editor to run for, if any.
pub fn seconds_from_environment() -> Option<f32> {
    env::var(SECONDS_ENVIRONMENT_VARIABLE)
        .ok()
        .and_then(|value| value.parse().ok())
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_parse_into_commands() {
        assert_eq!(
            parse_command("click 100 250.5"),
            TestCommand::Click { x: 100.0, y: 250.5 }
        );
        assert_eq!(
            parse_command("key Escape"),
            TestCommand::Key {
                key: "escape".to_string(),
                target: None,
            }
        );
        assert_eq!(
            parse_command("key W Scene"),
            TestCommand::Key {
                key: "w".to_string(),
                target: Some("scene".to_string()),
            }
        );
        assert_eq!(
            parse_command("tab Game"),
            TestCommand::Tab {
                title: "Game".to_string()
            }
        );
        assert_eq!(
            parse_command("command editor.pause_toggle"),
            TestCommand::Command {
                id: "editor.pause_toggle".to_string()
            }
        );
        assert_eq!(
            parse_command("chord ctrl+shift+p"),
            TestCommand::Chord {
                text: "ctrl+shift+p".to_string()
            }
        );
        assert_eq!(parse_command("quit"), TestCommand::Quit);
        assert!(matches!(
            parse_command("click nope"),
            TestCommand::Unknown { .. }
        ));
        assert!(matches!(
            parse_command("dance"),
            TestCommand::Unknown { .. }
        ));
    }
}
