//! Console panel: command failures surfaced by the last apply batch.
//!
//! # Responsibilities
//!
//! - Render the bounded ring buffer of editor-command failures held on
//!   [`EditorContext`](crate::EditorContext), newest first.
//! - Stay a pure view: it never touches the engine itself.

use std::sync::Arc;
use std::time::Duration;

use dioxus::prelude::*;

use crate::editor_state::EditorSnapshot;
use crate::EditorContext;

/// Poll interval for the local snapshot copy.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The Console panel body.
#[component]
pub(crate) fn ConsoleTab(editor: Arc<EditorContext>) -> Element {
    let mut snapshot = use_signal(EditorSnapshot::default);

    let poll_editor = Arc::clone(&editor);
    use_future(move || {
        let poll_editor = Arc::clone(&poll_editor);
        async move {
            loop {
                tokio::time::sleep(POLL_INTERVAL).await;
                snapshot.set(poll_editor.snapshot());
            }
        }
    });

    let errors = snapshot.read().errors.clone();
    rsx! {
        div {
            class: "editor-panel editor-console",
            if errors.is_empty() {
                div { class: "editor-placeholder", "No command errors." }
            } else {
                ul {
                    class: "editor-list",
                    // Newest command failure first, like a ring buffer head.
                    for (index, message) in errors.iter().rev().enumerate() {
                        li {
                            key: "console-error-{index}",
                            class: "editor-row-subtitle",
                            "{message}"
                        }
                    }
                }
            }
        }
    }
}
