//! The asks one shell pass raises for the editor executable.
//!
//! # Responsibilities
//!
//! - [`ShellRequests`]: the one-shot asks a pass raises for the executable - a
//!   Problems row to open, an undo or redo, a dock tab to show - taken in one
//!   call at the frame boundary.
//! - [`HistoryRequest`]: which of the two history walks an ask names.
//!
//! # Design
//!
//! The shell module and the executable are separate images, so the asks cross
//! as plain data: an index, a small enum and a tab, all defined here in the
//! shared `dylib`. The pass writes them while it draws; `settle_pass` takes
//! the whole value onto [`crate::boundary::EguiShellFrame`] and the executable
//! drains it from the frame, which is already the outbound channel for its
//! quit request and a dragged-out asset.
//!
//! Taking the value in one call is the contract: [`ShellRequests::drain`]
//! hands back every ask and leaves the resource empty, so a caller cannot
//! answer one and silently leave another behind.
//!
//! A request that must outlive the pass that raised it does not belong here.
//! The scripted channel's panel-window ask waits on the shell session until
//! the pinged panel draws, and a dock edit the pass cannot make while the
//! dock is borrowed rides the shell's own settle-stage value instead.

// Current crate
use crate::layout::EditorTab;

/// The one-shot asks one shell pass raises for the editor executable.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShellRequests {
    /// The Problems row a click asked to open: an index into the editor's
    /// problem list, which only the executable can resolve to a `file:line`.
    pub problem_open: Option<usize>,
    /// The undo and redo steps the menu bar asked for, oldest first: each is
    /// one recorded step, so their order is the order they apply in.
    pub history: Vec<HistoryRequest>,
    /// The dock tab a menu item asked to show. A tab the dock no longer holds
    /// is re-docked by the executable rather than dropped.
    pub pending_tab: Option<EditorTab>,
}

impl ShellRequests {
    /// Take every request at once, leaving the resource empty.
    ///
    /// One call, one boundary: the caller sees every ask the pass raised and
    /// answers them in one phase, so no request is lost between two `take`s.
    #[must_use]
    pub fn drain(&mut self) -> Self {
        std::mem::take(self)
    }

    /// Whether the pass raised nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.problem_open.is_none() && self.history.is_empty() && self.pending_tab.is_none()
    }
}

/// An undo or redo the menu bar asked for, drained by the executable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryRequest {
    /// Undo the newest recorded step.
    Undo,
    /// Redo the newest undone step.
    Redo,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_drain_returns_every_slot_and_clears_them_all() {
        let mut requests = ShellRequests {
            problem_open: Some(3),
            history: vec![HistoryRequest::Undo, HistoryRequest::Redo],
            pending_tab: Some(EditorTab::Assets),
        };
        let drained = requests.drain();
        assert_eq!(drained.problem_open, Some(3));
        assert_eq!(
            drained.history,
            vec![HistoryRequest::Undo, HistoryRequest::Redo]
        );
        assert_eq!(drained.pending_tab, Some(EditorTab::Assets));
        assert!(requests.is_empty());
        assert_eq!(requests, ShellRequests::default());
    }

    #[test]
    fn an_empty_value_is_empty() {
        assert!(ShellRequests::default().is_empty());
    }
}
