//! One polling hook for the editor's panels.
//!
//! # Responsibilities
//!
//! - Own the clone-and-loop protocol every panel uses to copy shared editor
//!   state into its local signals.
//! - Keep interval and cancellation policy in one place: the future is
//!   recreated when the component remounts and dioxus cancels the old one, so
//!   a loop can end only by that cancellation.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use dioxus::prelude::use_future;

use crate::EditorContext;

/// Poll the shared editor context on an interval, for one panel's signals.
///
/// Each panel passes its own interval and a `tick` closure that reads what it
/// needs from the context - the shared snapshot, the registered component
/// list, the asset tree - and sets its own signals. The `Arc` is cloned into
/// the loop rather than captured by the caller, so the outer clone surviving
/// a remount is the hook's business rather than each panel's.
pub(crate) fn use_poll_editor(
    editor: &Arc<EditorContext>,
    interval: Duration,
    tick: impl FnMut(&EditorContext) + 'static,
) {
    let poll_editor = Arc::clone(editor);
    // `use_future` takes an `FnMut` closure and may call it again on a
    // remount, so the setter is shared rather than captured by value; the
    // spawned future owns one handle of it.
    let tick = Rc::new(RefCell::new(tick));
    use_future(move || {
        let poll_editor = Arc::clone(&poll_editor);
        let tick = Rc::clone(&tick);
        async move {
            loop {
                tokio::time::sleep(interval).await;
                (tick.borrow_mut())(&poll_editor);
            }
        }
    });
}
