//! The Console tab's history and filters.
//!
//! # Responsibilities
//!
//! - Hold the lines the executable captures: bounded, owned, oldest first, one
//!   row per engine or project log line.
//! - Split every line into the engine channel and the game channel, so a
//!   project's own messages can be read apart from the engine's.
//! - Own the filter state (lowest level shown, substring search, channel) the
//!   shell's tab draws and the scripted channel sets.
//!
//! # Design
//!
//! A world resource, filled by the executable each frame from the telemetry
//! capture ring and read by the shell's tab. Every string is owned: the
//! capture ring lives in `pill_core`, but the tab outlives any module image
//! that happened to log the line, and a borrowed target would dangle across a
//! reload.

// External crates
use std::collections::VecDeque;

// Current crate
use crate::notices::NoticeLevel;

/// How many lines the history keeps; older ones drop off.
pub const CONSOLE_LINES_MAX: usize = 2000;

/// The prefixes this engine's own code logs under.
///
/// Every crate in this workspace is named `pill_*`, and the engine's own
/// tracing roots are `engine::…` and `profile::…`. A target's first segment is
/// matched against these prefixes, so `pill_master_renderer_data::scene` and
/// `pill_engine::asset::AssetManager` are engine output, while
/// `master_renderer_test::controls` - the renderer example's own crate - is the
/// game's.
///
/// The one boundary the rule has: a project crate that names itself `pill_*`
/// is filed as the engine's, exactly as one named `engine_tools` would be
/// filed by the `engine` prefix. That is a naming collision, not a
/// classification bug, and it is visible here rather than hidden in a
/// substring test.
pub const ENGINE_PREFIXES: [&str; 3] = ["engine", "pill_", "profile"];

/// Which side of the editor a line came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConsoleChannel {
    /// The engine, the host and the editor itself.
    Engine,
    /// The project: its modules and its scripts.
    #[default]
    Game,
}

impl ConsoleChannel {
    /// The channel's name in the tab's status row and in the log.
    pub fn name(self) -> &'static str {
        match self {
            ConsoleChannel::Engine => "engine",
            ConsoleChannel::Game => "game",
        }
    }

    /// The channel a target belongs to.
    ///
    /// The engine, the host, the editor and the renderers own a fixed set of
    /// crate prefixes ([`ENGINE_PREFIXES`]); a project's crate is anything
    /// else, whatever its name happens to contain. The distinction is by whole
    /// prefix, not by substring, so the repository's own renderer example -
    /// whose crate is `master_renderer_test` - logs into its own channel
    /// instead of being filed as engine output by the `render` it contains.
    pub fn of_target(target: &str) -> Self {
        let head = target.split("::").next().unwrap_or(target);
        let engine_owned = ENGINE_PREFIXES
            .iter()
            .any(|prefix| head.starts_with(prefix));
        if engine_owned {
            ConsoleChannel::Engine
        } else {
            ConsoleChannel::Game
        }
    }
}

/// One console row, owned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsoleLine {
    /// The capture ring's sequence number; the feed reads past it.
    pub seq: u64,
    /// Severity, 1 trace to 5 error.
    pub level: u8,
    /// The tracing target the line came from.
    pub target: String,
    /// The formatted message.
    pub message: String,
    /// Which channel the target put it in.
    pub channel: ConsoleChannel,
}

impl ConsoleLine {
    /// The level's wire name (`info`, `warn`, ...).
    pub fn level_name(&self) -> &'static str {
        level_name(self.level)
    }
}

/// The wire name of a capture level.
pub fn level_name(level: u8) -> &'static str {
    match level {
        0..=1 => "trace",
        2 => "debug",
        3 => "info",
        4 => "warn",
        _ => "error",
    }
}

/// The capture level a name asks for, for the scripted filter verb.
pub fn level_from_name(name: &str) -> Option<u8> {
    match name {
        "trace" => Some(1),
        "debug" => Some(2),
        "info" => Some(3),
        "warn" => Some(4),
        "error" => Some(5),
        _ => None,
    }
}

/// The console's history and read state.
#[derive(Clone, Debug)]
pub struct ConsoleState {
    /// The kept lines, oldest first, capped at [`CONSOLE_LINES_MAX`].
    pub lines: VecDeque<ConsoleLine>,
    /// The newest capture sequence already folded in.
    pub last_seq: u64,
    /// The lowest level shown: 3 hides trace and debug.
    pub minimum_level: u8,
    /// A case-insensitive substring the message or target must contain.
    pub search: String,
    /// Which channel to show; `None` shows both.
    pub channel: Option<ConsoleChannel>,
}

impl Default for ConsoleState {
    fn default() -> Self {
        Self {
            lines: VecDeque::new(),
            last_seq: 0,
            minimum_level: 1,
            search: String::new(),
            channel: None,
        }
    }
}

impl pill_engine::Resource for ConsoleState {}

/// How many rows a [`ConsoleView`] carries: the tab draws these and pages
/// through history from them.
pub const CONSOLE_VIEW_ROWS: usize = 200;

/// What the Console tab draws: the filters in force and the rows they leave.
///
/// A copy, like the menus' command rows: the shell's tab reads the frame's
/// snapshot and never the world.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConsoleView {
    /// The lowest level shown.
    pub minimum_level: u8,
    /// The search text in force.
    pub search: String,
    /// The channel in force; `None` shows both.
    pub channel: Option<ConsoleChannel>,
    /// How many lines the history holds.
    pub total: usize,
    /// How many lines the filters leave.
    pub visible: usize,
    /// The newest visible rows, oldest first.
    pub lines: Vec<ConsoleLine>,
}

/// The filters the Console tab edits.
///
/// They live in the shell's session, not in [`ConsoleState`]: the tab draws
/// from the frame's snapshot and cannot reach the world, so it edits this
/// copy and the executable applies it to the history on the next frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsoleFilterSettings {
    /// The lowest level shown.
    pub minimum_level: u8,
    /// The search text.
    pub search: String,
    /// The channel, or `None` for both.
    pub channel: Option<ConsoleChannel>,
}

impl Default for ConsoleFilterSettings {
    fn default() -> Self {
        Self {
            minimum_level: 1,
            search: String::new(),
            channel: None,
        }
    }
}

impl ConsoleState {
    /// Fold one captured line in, dropping the oldest beyond the cap.
    pub fn push(&mut self, line: ConsoleLine) {
        self.last_seq = self.last_seq.max(line.seq);
        self.lines.push_back(line);
        while self.lines.len() > CONSOLE_LINES_MAX {
            self.lines.pop_front();
        }
    }

    /// The lines the filters leave, oldest first.
    pub fn visible(&self) -> impl Iterator<Item = &ConsoleLine> {
        let needle = self.search.trim().to_lowercase();
        self.lines.iter().filter(move |line| {
            line.level >= self.minimum_level
                && self.channel.is_none_or(|channel| line.channel == channel)
                && (needle.is_empty()
                    || line.message.to_lowercase().contains(&needle)
                    || line.target.to_lowercase().contains(&needle))
        })
    }

    /// How many lines the filters leave.
    pub fn visible_count(&self) -> usize {
        self.visible().count()
    }

    /// Apply a filter change the shell asked for.
    pub fn apply(&mut self, settings: &ConsoleFilterSettings) {
        self.minimum_level = settings.minimum_level;
        self.search = settings.search.clone();
        self.channel = settings.channel;
    }

    /// The filters in force, as the session stores them.
    pub fn settings(&self) -> ConsoleFilterSettings {
        ConsoleFilterSettings {
            minimum_level: self.minimum_level,
            search: self.search.clone(),
            channel: self.channel,
        }
    }

    /// The tab's view: the filters and the newest rows they leave.
    pub fn view(&self) -> ConsoleView {
        let visible: Vec<&ConsoleLine> = self.visible().collect();
        let start = visible.len().saturating_sub(CONSOLE_VIEW_ROWS);
        ConsoleView {
            minimum_level: self.minimum_level,
            search: self.search.clone(),
            channel: self.channel,
            total: self.lines.len(),
            visible: visible.len(),
            lines: visible[start..]
                .iter()
                .map(|line| (*line).clone())
                .collect(),
        }
    }

    /// The notice level a capture level maps to, for logs that report a line.
    pub fn notice_level(level: u8) -> NoticeLevel {
        match level {
            4 => NoticeLevel::Warning,
            5 => NoticeLevel::Error,
            _ => NoticeLevel::Note,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One line at `level` from `target`.
    fn line(seq: u64, level: u8, target: &str, message: &str) -> ConsoleLine {
        ConsoleLine {
            seq,
            level,
            target: target.to_string(),
            message: message.to_string(),
            channel: ConsoleChannel::of_target(target),
        }
    }

    #[test]
    fn engine_targets_and_project_targets_land_in_their_channels() {
        // The engine, host, editor, renderer and profiler crates own their
        // prefixes...
        for target in [
            "engine::ecs",
            "engine::rendering",
            "pill_core::platform",
            "pill_engine::asset::AssetManager",
            "pill_host::loader",
            "pill_editor_core::menu",
            "pill_editor::runner",
            "pill_master_renderer_data::scene",
            "pill_renderer_api::limits",
            "profile::coarse",
        ] {
            assert_eq!(
                ConsoleChannel::of_target(target),
                ConsoleChannel::Engine,
                "{target} is one of the engine's own crates"
            );
        }
        // ...and a project's crate is the game's whatever its name contains:
        // `master_renderer_test` is the renderer example, whose lines used to
        // be filed as engine output because the name holds `render`, and
        // `acme_editor` is a project-side extension.
        for target in [
            "italian_brainrot::game",
            "project_rs::systems",
            "master_renderer_test::controls",
            "render_demo::controls",
            "acme_editor::panel",
            "head_circus::tick",
        ] {
            assert_eq!(
                ConsoleChannel::of_target(target),
                ConsoleChannel::Game,
                "{target} is a project's crate"
            );
        }
    }

    #[test]
    fn the_history_stays_bounded() {
        let mut state = ConsoleState::default();
        for seq in 1..=(CONSOLE_LINES_MAX as u64 + 10) {
            state.push(line(seq, 3, "engine::ecs", "tick"));
        }
        assert_eq!(state.lines.len(), CONSOLE_LINES_MAX);
        assert_eq!(state.last_seq, CONSOLE_LINES_MAX as u64 + 10);
        // The oldest lines are the ones that dropped.
        assert_eq!(state.lines.front().map(|row| row.seq), Some(11));
    }

    #[test]
    fn the_filters_narrow_by_level_search_and_channel() {
        let mut state = ConsoleState::default();
        state.push(line(1, 3, "engine::ecs", "reload finished"));
        state.push(line(2, 4, "engine::ecs", "shader warning"));
        state.push(line(3, 3, "project_rs::game", "spawned a cube"));
        assert_eq!(state.visible_count(), 3);

        state.minimum_level = 4;
        assert_eq!(state.visible_count(), 1);

        state.minimum_level = 1;
        state.search = "CUBE".to_string();
        assert_eq!(state.visible_count(), 1);

        state.search.clear();
        state.channel = Some(ConsoleChannel::Engine);
        assert_eq!(state.visible_count(), 2);
    }
}
