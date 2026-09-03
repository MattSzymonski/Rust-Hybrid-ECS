//! Hierarchy panel: a live flat list of the running project's entities.
//!
//! # Responsibilities
//!
//! - List every live entity with its component counts and archetype label.
//! - Track the Inspector selection and offer create/delete actions.
//!
//! The engine has no entity names or parenting, so this is an honest flat list
//! of generation-tagged handles with their component counts. Selection lives on
//! [`EditorContext`](crate::EditorContext) so the Inspector in another dock (or
//! a pop-out window) can read it.

use std::sync::Arc;
use std::time::Duration;

use dioxus::prelude::*;
use pill_engine::Entity;

use crate::editor_state::{EditorCommand, EditorSnapshot};
use crate::EditorContext;

/// Poll interval for the local snapshot copy, matching the refresh cadence.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The Hierarchy panel body.
#[component]
pub(crate) fn EntitiesTab(editor: Arc<EditorContext>) -> Element {
    let mut snapshot = use_signal(EditorSnapshot::default);
    let mut context_menu = use_signal(|| None::<Entity>);

    // Every VirtualDom polls the shared context into its own signal, exactly
    // like the detached-window stats loop, so this component works identically
    // in the main dock and in a pop-out.
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

    // Clicking a row selects it for the Inspector; right-click opens the
    // entity context menu.
    let entities = snapshot.read().entities.clone();
    rsx! {
        div {
            class: "editor-panel editor-hierarchy",
            div {
                class: "editor-panel-toolbar",
                button {
                    onclick: {
                        let editor = Arc::clone(&editor);
                        move |_| {
                            editor.push_command(EditorCommand::CreateEntity {
                                components: Vec::new(),
                            });
                        }
                    },
                    "New entity"
                }
            }
            ul {
                class: "editor-list",
                for entry in entities {
                    li {
                        key: "entity-{entry.entity.id()}-{entry.entity.generation()}",
                        class: "editor-list-row",
                        onclick: {
                            let editor = Arc::clone(&editor);
                            move |_| editor.set_selection(Some(entry.entity))
                        },
                        oncontextmenu: move |event| {
                            event.prevent_default();
                            context_menu.set(Some(entry.entity));
                        },
                        div { class: "editor-row-title", "{entry.display}" }
                        div {
                            class: "editor-row-subtitle",
                            "{entry.component_count} component(s): {entry.archetype_label}"
                        }
                    }
                }
            }
        }
        if let Some(entity) = context_menu() {
            div {
                class: "editor-context-menu",
                onclick: move |_| context_menu.set(None),
                button {
                    onclick: {
                        let editor = Arc::clone(&editor);
                        move |_| {
                            editor.push_command(EditorCommand::DestroyEntity { entity });
                            context_menu.set(None);
                        }
                    },
                    "Delete entity"
                }
            }
        }
    }
}
