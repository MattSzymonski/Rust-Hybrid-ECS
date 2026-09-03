//! Systems panel: registered systems with owner labels, an enable toggle, and
//! the pause/step transport controls that make inspector edits observable.
//!
//! # Responsibilities
//!
//! - List every registered system with its owner label and patchability.
//! - Toggle systems by registration index and drive pause/single-step.

use std::sync::Arc;
use std::time::Duration;

use dioxus::prelude::*;

use crate::editor_state::{EditorCommand, EditorSnapshot};
use crate::EditorContext;

/// Poll interval for the local snapshot copy.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The Systems panel body.
#[component]
pub(crate) fn SystemsTab(editor: Arc<EditorContext>) -> Element {
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

    let systems = snapshot.read().systems.clone();
    let paused = snapshot.read().paused;

    // Flatten systems into pre-formatted rows so rsx stays declarative.
    struct SystemRow {
        index: usize,
        name: String,
        subtitle: String,
        ambiguous: bool,
        enabled: bool,
    }
    let rows: Vec<SystemRow> = systems
        .into_iter()
        .map(|system| {
            let mechanism = if system.hot_patchable {
                "hot-patchable"
            } else {
                "module-reload"
            };
            SystemRow {
                index: system.index,
                name: system.name,
                subtitle: format!("{} · {mechanism}", system.owner_label),
                ambiguous: system.name_is_ambiguous,
                enabled: system.enabled,
            }
        })
        .collect();

    rsx! {
        div {
            class: "editor-panel editor-systems",
            div {
                class: "editor-panel-toolbar",
                button {
                    onclick: {
                        let editor = Arc::clone(&editor);
                        move |_| {
                            editor.push_command(EditorCommand::SetPaused { paused: !paused });
                        }
                    },
                    if paused { "Resume" } else { "Pause" }
                }
                button {
                    onclick: {
                        let editor = Arc::clone(&editor);
                        move |_| editor.push_command(EditorCommand::StepOnce)
                    },
                    "Step"
                }
            }
            ul {
                class: "editor-list",
                for row in rows {
                    li {
                        key: "system-{row.index}",
                        class: "editor-list-row",
                        div {
                            class: "editor-row-title",
                            "{row.name}",
                            if row.ambiguous {
                                span { class: "editor-tag editor-warn", "ambiguous name" }
                            }
                        }
                        div {
                            class: "editor-row-subtitle",
                            "{row.subtitle}"
                        }
                        input {
                            class: "editor-checkbox",
                            r#type: "checkbox",
                            checked: row.enabled,
                            onchange: {
                                let editor = Arc::clone(&editor);
                                let enabled = !row.enabled;
                                move |_| {
                                    editor.push_command(EditorCommand::SetSystemEnabled {
                                        index: row.index,
                                        enabled,
                                    });
                                }
                            },
                        }
                    }
                }
            }
        }
    }
}
