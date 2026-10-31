//! What the host did, as plain data for the egui shell: toasts and problems.
//!
//! # Responsibilities
//!
//! - Define [`EditorNotices`]: the resource the executable writes before the
//!   shell's pass - the toast events and the Problems rows its drain of the
//!   host's notice queue produced.
//! - Define [`Notice`], [`NoticeLevel`] and [`ProblemRow`]: owned strings only.
//! - Name a level for the log lines the suites match ([`level_name`]).
//!
//! # Design
//!
//! `pill_development_host::HostNotice` says what the host did - a module reloaded, a patch
//! went live or failed, a function rolled back. The executable folds each one
//! into a toast line, and the failures also into a Problems row for their
//! module; the shell only reads. The host owns the notice queue, the
//! executable owns this resource, and the shell draws from it, which is why it
//! lives in this shared `dylib` rather than in the reloadable shell module: its
//! identity survives a shell generation, and no field may point into a module
//! image the reload unmaps.

// External crates
use pill_engine::Resource;

// Current crate
use crate::problems::Problem;

/// How serious one toast or problem is.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum NoticeLevel {
    /// A build or a patch failed; the previous generation keeps running.
    Error,
    /// Something worth a second look: a rollback, a compiler warning.
    Warning,
    /// A plain notice: a reload finished, a patch went live.
    Note,
}

impl NoticeLevel {
    /// The level's wire name, for the log line the suites match.
    pub fn name(self) -> &'static str {
        level_name(self)
    }
}

/// The wire name of a level: `error`, `warning` or `info`.
pub fn level_name(level: NoticeLevel) -> &'static str {
    match level {
        NoticeLevel::Error => "error",
        NoticeLevel::Warning => "warning",
        NoticeLevel::Note => "info",
    }
}

/// One toast line: something the host did, shown for a few seconds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    /// A per-session monotonic id; the shell's toast list keys on it, so a
    /// redrawn frame never re-queues a line it already shows.
    pub seq: u64,
    /// How serious it is.
    pub level: NoticeLevel,
    /// The line to show.
    pub text: String,
}

/// One Problems row: the module whose build, load or patch reported it, and
/// the diagnostic itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProblemRow {
    /// The module, function or patch that reported it.
    pub module: String,
    /// The diagnostic, with the location the compiler printed.
    pub problem: Problem,
}

/// The notices the executable publishes for the shell each frame.
#[derive(Clone, Debug, Default)]
pub struct EditorNotices {
    /// Bumped whenever `problems` or `events` changed; the shell reacts to a
    /// new value instead of diffing the lists.
    pub revision: u64,
    /// The current problems, one row per diagnostic, unsorted: `module` names
    /// the owner. A module that reloads cleanly drops its rows.
    pub problems: Vec<ProblemRow>,
    /// The toast events the shell consumes by [`Notice::seq`].
    pub events: Vec<Notice>,
    /// The id `events`' next entry takes.
    pub next_event_seq: u64,
}

impl Resource for EditorNotices {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_name_themselves_for_the_log() {
        assert_eq!(NoticeLevel::Error.name(), "error");
        assert_eq!(NoticeLevel::Warning.name(), "warning");
        assert_eq!(NoticeLevel::Note.name(), "info");
        // The free function and the method agree, because the log line and
        // the suites both go through one mapping.
        assert_eq!(level_name(NoticeLevel::Note), NoticeLevel::Note.name());
    }
}
