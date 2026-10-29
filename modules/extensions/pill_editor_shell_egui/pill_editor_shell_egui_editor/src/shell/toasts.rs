//! The toast stack: what the host did, drawn over the editor for a few
//! seconds.
//!
//! # Responsibilities
//!
//! - [`push_notices`]: queue each new notice event as a toast, newest last,
//!   capped at [`TOASTS_MAX`].
//! - [`draw_toasts`]: expire, place and draw the stack in the window's
//!   top-left corner; log each toast once; publish each row's rectangle so a
//!   click on a toast can never be a scene pick.
//!
//! # Design
//!
//! The executable folds `pill_host::HostNotice`s into
//! [`EditorNotices`-shaped plain data][EditorNotices] each frame; this module
//! only reads it and turns it into pixels. A toast is keyed by its notice
//! sequence number, so a redrawn frame neither re-queues a line nor repeats
//! its log entry, and a shell reload that keeps [`ToastSession`] keeps the
//! stack on screen.

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;
// Deliberately unused in this compilation: a patch crate replays this
// file's opening, and the first sibling this body names resolves through it.
#[allow(unused_imports)]
use pill_editor_shell_egui_editor::shell::*;

/// Where the stack's first row sits, just below the menu bar.
const TOAST_TOP: f32 = menubar::MENU_BAR_HEIGHT + 6.0;
/// The stack's inset from the window's left edge.
const TOAST_LEFT: f32 = 8.0;
/// One row's width and height in logical points.
const TOAST_WIDTH: f32 = 420.0;
const TOAST_HEIGHT: f32 = 24.0;
/// The gap between two rows.
const TOAST_GAP: f32 = 4.0;

/// Queue every notice the stack has not seen as a toast.
///
/// The newest notice sits at the top of the stack, so the most recent activity
/// is the first row read.
pub fn push_notices(state: &mut EguiShellState, notices: &EditorNotices) {
    let mut newest = state.toasts.last_seq;
    for event in &notices.events {
        let fresh = state.toasts.last_seq.is_none_or(|last| event.seq > last);
        if !fresh {
            continue;
        }
        if state.toasts.toasts.len() >= TOASTS_MAX {
            state.toasts.toasts.pop_front();
        }
        state.toasts.toasts.push_back(Toast {
            seq: event.seq,
            level: event.level,
            text: event.text.clone(),
            expires_at_frame: state.frame_index + TOAST_FRAMES,
        });
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ENGINE,
            seq = event.seq,
            level = event.level.name(),
            "[editor] toast queued"
        );
        newest = Some(event.seq);
    }
    state.toasts.last_seq = newest;
}

/// Draw the live toasts and publish their rectangles.
///
/// Called from inside the shell's egui pass, after the dock: a toast floats
/// over whatever it covers, and a click on one belongs to the UI.
pub fn draw_toasts(
    toasts: &mut ToastSession,
    frame_index: u64,
    context: &egui::Context,
    regions: &mut Vec<ShellRect>,
) {
    toasts
        .toasts
        .retain(|toast| toast.expires_at_frame > frame_index);
    if toasts.toasts.is_empty() {
        toasts.logged.clear();
        return;
    }
    // Collect the rows first: the loop below mutates the log bookkeeping, and
    // the newest row goes on top.
    let rows: Vec<(u64, NoticeLevel, String)> = toasts
        .toasts
        .iter()
        .rev()
        .map(|toast| (toast.seq, toast.level, toast.text.clone()))
        .collect();
    let mut on_screen = Vec::with_capacity(rows.len());
    for (index, (seq, level, text)) in rows.into_iter().enumerate() {
        let top = TOAST_TOP + index as f32 * (TOAST_HEIGHT + TOAST_GAP);
        let area = egui::Area::new(egui::Id::new(("editor-toast", seq)))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(TOAST_LEFT, top))
            .show(context, |ui| {
                egui::Frame::new()
                    .fill(style::panel())
                    .stroke(egui::Stroke::new(1.0, style::line()))
                    .inner_margin(egui::Margin::symmetric(8, 2))
                    .show(ui, |ui| {
                        ui.set_width(TOAST_WIDTH);
                        // The level's accent bar down the left edge.
                        let bar = egui::Rect::from_min_size(
                            ui.max_rect().left_top(),
                            egui::vec2(3.0, ui.max_rect().height()),
                        );
                        ui.painter().rect_filled(
                            bar,
                            egui::CornerRadius::ZERO,
                            level_colour(level),
                        );
                        ui.horizontal(|ui| {
                            ui.colored_label(level_colour(level), level.name());
                            ui.label(egui::RichText::new(&text).color(style::text()));
                        });
                    });
            });
        let rect = area.response.rect;
        regions.push(ShellRect {
            x: rect.min.x,
            y: rect.min.y,
            width: rect.width(),
            height: rect.height(),
        });
        on_screen.push(seq);
        // One line per toast, not one per frame: the suite reads the level,
        // the text and where the row landed.
        if !toasts.logged.contains(&seq) {
            toasts.logged.push(seq);
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
                level = level.name(),
                seq,
                text = text.as_str(),
                x = rect.min.x,
                y = rect.min.y,
                width = rect.width(),
                height = rect.height(),
                "[editor] toast"
            );
        }
    }
    toasts.logged.retain(|seq| on_screen.contains(seq));
}

/// The colour of a level's marker.
pub fn level_colour(level: NoticeLevel) -> egui::Color32 {
    match level {
        NoticeLevel::Error => style::log_error(),
        NoticeLevel::Warning => style::log_warn(),
        NoticeLevel::Note => style::log_info(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session with a stack, for the queueing tests.
    fn session() -> EguiShellState {
        let mut state = EguiShellState::new();
        state.frame_index = 100;
        state
    }

    #[test]
    fn each_notice_is_queued_once_and_expires() {
        let mut state = session();
        let notices = EditorNotices {
            events: vec![Notice {
                seq: 0,
                level: NoticeLevel::Note,
                text: "spline reloaded in 120 ms".to_string(),
            }],
            next_event_seq: 1,
            ..Default::default()
        };
        push_notices(&mut state, &notices);
        assert_eq!(state.toasts.toasts.len(), 1);
        let toast = &state.toasts.toasts[0];
        assert_eq!(toast.expires_at_frame, 100 + TOAST_FRAMES);
        // The same frame's list again queues nothing: the id was consumed.
        push_notices(&mut state, &notices);
        assert_eq!(state.toasts.toasts.len(), 1);
    }

    #[test]
    fn the_stack_keeps_the_newest_toasts() {
        let mut state = session();
        let notices = EditorNotices {
            events: (0..(TOASTS_MAX as u64 + 2))
                .map(|seq| Notice {
                    seq,
                    level: NoticeLevel::Note,
                    text: format!("notice {seq}"),
                })
                .collect(),
            next_event_seq: TOASTS_MAX as u64 + 2,
            ..Default::default()
        };
        push_notices(&mut state, &notices);
        assert_eq!(state.toasts.toasts.len(), TOASTS_MAX);
        // The two oldest are the ones that dropped off.
        assert_eq!(state.toasts.toasts[0].seq, 2);
    }
}
