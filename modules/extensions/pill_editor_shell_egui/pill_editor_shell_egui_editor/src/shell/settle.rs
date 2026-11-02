//! The stage that runs after a shell pass.
//!
//! # Responsibilities
//!
//! - [`FrameRequests`]: the one value a pass's tabs and menus write where they
//!   raise a deferred request - the dock edits they cannot make while the dock
//!   is borrowed, a drag that left the windows, a quit.
//! - [`settle_pass`]: applies those requests in the order the pass requires,
//!   and returns the frame the pass produced.
//!
//! # Design
//!
//! The ordering inside this stage is load-bearing, which is why the stage is
//! one function instead of blocks scattered through `run_frame`:
//!
//! 1. popped-out tabs whose tab left the popout set lose their context first -
//!    a context nothing draws into would otherwise be cleared on every
//!    generation's first frame and kept for the session's life;
//! 2. each remaining popped-out tab runs its own pass with the same viewer,
//!    because its window has its own widget state and input record;
//! 3. a tab menu's pop-out runs after those passes, so a tab that asked to
//!    leave is drawn in the dock for the rest of this pass;
//! 4. a re-dock runs after the pop-out, because both edit the dock tree;
//! 5. a new scene view is docked after both and activated through the
//!    executable's own path, so the new tab is at the front of its group the
//!    moment it appears;
//! 6. a panel window's answer is cleared only after a pass that read it;
//! 7. the scripted window request is cleared only when the pinged panel drew;
//! 8. the budget toasts go out last, after the viewer's final use - the viewer
//!    holds the budget mutably while it draws;
//! 9. the asks the pass raised for the executable - a Problems row, undo/redo
//!    and the tab step 5 picked - leave on the frame, together with the drag
//!    path and the menu's quit, one take clearing them all.
//!
//! Nothing here draws: every step edits state the next frame reads, which is
//! what keeps the stage safe to run at the boundary.

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;
// Deliberately unused in this compilation: a patch crate replays this file's
// opening, and the first sibling this body names resolves through it.
#[allow(unused_imports)]
use pill_editor_shell_egui_editor::shell::*;

/// What one shell pass could not act on while the dock and its tabs were
/// borrowed.
///
/// Every field is a request raised during the pass: a tab that asked to pop
/// out or come back, a menu item that asked for one more scene view or a
/// quit, the drag that left the windows. The tabs and menus write the value
/// where the request is raised, through `EditorTabViewer::deferred`; the
/// settle stage reads it back from the same place, so a request has one home
/// and needs no copy between the pass and its settle stage.
#[derive(Default)]
pub struct FrameRequests {
    /// The asset path a tile drag handed to the OS; the executable starts an
    /// OLE drag with it, because the shell cannot.
    pub drag_out: Option<String>,
    /// The tab a tab menu asked to move out into its own window.
    pub tab_popout: Option<EditorTab>,
    /// The surface a tab menu asked to bring back into the dock.
    pub tab_redock: Option<SurfaceIndex>,
    /// Whether the Window menu asked for one more scene view.
    pub new_scene_view: bool,
    /// Whether a menu asked the editor to quit; the executable honours it
    /// through the same path a window close takes.
    pub quit_requested: bool,
}

/// Apply one pass's deferred requests and return the frame it produced.
///
/// The order of the steps is the contract; see the module docs for why each
/// one sits where it does. `viewer` is the pass's own viewer: the popped-out
/// windows draw through it, and it holds the panel budget mutably until the
/// last step.
// Ten collaborators, and they are distinct borrows of the shell's state
// taken apart for exactly this call: grouping them would name a type that
// exists only to satisfy the lint and would hide which of them this stage
// mutates.
#[allow(clippy::too_many_arguments)]
pub fn settle_pass(
    dock: &mut pill_editor_core::layout::EditorDock,
    popout_contexts: &mut std::collections::BTreeMap<EditorTab, egui::Context>,
    popout_inputs: &pill_editor_core::boundary::EguiPopoutInputs,
    windows: &mut PanelWindowSession,
    window_ping: &mut Option<(u16, String)>,
    toasts: &mut ToastSession,
    panels: &PanelRegistry,
    frame_index: u64,
    viewer: &mut EditorTabViewer<'_>,
    mut shell_frame: EguiShellFrame,
) -> EguiShellFrame {
    // Step 1: a tab that came back to the dock takes its context with it.
    popout_contexts.retain(|tab, _| dock.is_popped_out(*tab));
    // Step 2: one more pass per popped-out tab - its own context, its own
    // widget state, its own frame.
    shell_frame.popout_frames = pill_editor_shell_egui_editor::shell::run_popout_passes(
        dock,
        popout_contexts,
        popout_inputs,
        viewer,
    );
    // Step 3: a tab menu's pop-out lands here for the same reason a re-dock
    // does - the dock was borrowed while it drew.
    if let Some(tab) = viewer.deferred.tab_popout {
        if dock.pop_out(tab) {
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
                tab = tab.name().as_str(),
                "[editor] popout moved"
            );
        }
    }
    // Step 4: changing the tree mid-draw would edit it under the widget that
    // asked.
    if let Some(surface) = viewer.deferred.tab_redock {
        pill_editor_shell_egui_editor::shell::apply_tab_redock(dock.state_mut(), surface);
    }
    // Step 5: the Window menu's "New Scene View", docked here for the same
    // reason and activated through the executable's own path.
    if viewer.deferred.new_scene_view {
        let tab = dock.next_scene_view();
        if dock.contains(tab) {
            // Already docked: the command brought it back rather than adding.
            viewer.requests.pending_tab = Some(tab);
        } else if dock.can_open_scene_view() {
            dock.state_mut().push_to_focused_leaf(tab);
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
                tab = tab.name().as_str(),
                view = tab.scene_view().unwrap_or_default(),
                "[editor] scene view added"
            );
            viewer.requests.pending_tab = Some(tab);
        } else {
            pill_core::warn!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
                limit = pill_editor_core::boundary::SCENE_VIEWS_MAX,
                "[editor] scene view refused: the view limit is reached"
            );
        }
    }
    // Step 6: the pass was offered an answer, so the next one must not act on
    // it again. The value lives on the viewer, which is the state that read
    // it - there is no second flag to keep in agreement.
    if viewer.panel_answer.is_some() {
        windows.answer = None;
    }
    // Step 7: the pending window's panel drew this pass, so its request is
    // spent.
    if viewer.pending_panel_window_used {
        *window_ping = None;
    }
    // Step 8: a panel that crossed its budget this frame gets its toast once,
    // named after the tab so the user can find the panel it belongs to. After
    // the viewer's last use: the viewer holds the budget mutably while it
    // draws.
    for entry in &mut viewer.panel_budget.entries {
        if !entry.warned || entry.toasted {
            continue;
        }
        entry.toasted = true;
        let title = panels.tab_title(entry.slot);
        let text = format!(
            "Panel {} is over its frame budget ({:.1} ms)",
            title,
            entry.micros / 1000.0
        );
        toasts.push_local(NoticeLevel::Warning, text, frame_index);
    }
    // Step 9: every ask this pass raised travels on the frame. One take clears
    // all of them at once, so a slot cannot be left behind by a caller that
    // answered one and forgot another. The viewer holds the value the pass
    // wrote it through.
    shell_frame.requests = std::mem::take(viewer.requests);
    // The tile drag's path and the menu's quit travel in the same frame the
    // executable drains.
    shell_frame.drag_out = viewer.deferred.drag_out.take();
    shell_frame.quit_requested = viewer.deferred.quit_requested;
    shell_frame
}
