//! Native pop-out windows for panels removed from the main dock tree.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dioxus::desktop::tao::dpi::LogicalSize;
use dioxus::desktop::tao::event::{Event as TaoEvent, WindowEvent};
use dioxus::desktop::tao::window::WindowBuilder;
use dioxus::desktop::{use_wry_event_handler, window, Config};
use dioxus::prelude::*;

use crate::dock_view::{live_dock_css, PanelContent};
use crate::layout::PanelKind;
use crate::{EditorContext, Stats};

/// Cross-VirtualDom mailbox for panels whose native windows have closed.
///
/// Each desktop window has an independent Dioxus runtime, so it cannot mutate
/// the main window's `Signal<LayoutModel>` directly. The main frame loop drains
/// this mailbox and performs the validated redock action in its own runtime.
#[derive(Default)]
pub(crate) struct PopoutManager {
    redock: Mutex<Vec<PanelKind>>,
}

impl PopoutManager {
    /// Queue a panel for reinsertion into the main dock tree.
    fn request_redock(&self, panel: PanelKind) {
        self.redock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(panel);
    }

    /// Drain all close notifications accumulated since the previous frame.
    pub(crate) fn drain_redocks(&self) -> Vec<PanelKind> {
        std::mem::take(
            &mut *self
                .redock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }
}

/// Immutable state owned by one detached panel's independent VirtualDom.
#[derive(Clone, Props)]
struct DetachedPanelProps {
    panel: PanelKind,
    editor: Arc<EditorContext>,
    popouts: Arc<PopoutManager>,
}

impl PartialEq for DetachedPanelProps {
    /// Window props are stable when they refer to the same shared services.
    fn eq(&self, other: &Self) -> bool {
        self.panel == other.panel
            && Arc::ptr_eq(&self.editor, &other.editor)
            && Arc::ptr_eq(&self.popouts, &other.popouts)
    }
}

/// Create a second OS window and mount the selected panel into its own WebView.
pub(crate) fn open_panel_window(
    panel: PanelKind,
    editor: Arc<EditorContext>,
    popouts: Arc<PopoutManager>,
) {
    let props = DetachedPanelProps {
        panel,
        editor: Arc::clone(&editor),
        popouts,
    };
    let dom = VirtualDom::new_with_props(DetachedPanelWindow, props);
    let mut config = Config::new().with_disable_context_menu(true).with_window(
        WindowBuilder::new()
            .with_title(format!("{} - ECS Editor", panel.title()))
            .with_inner_size(LogicalSize::new(720.0, 520.0))
            .with_transparent(panel == PanelKind::Scene),
    );

    if panel == PanelKind::Scene {
        // The renderer remains owned by the same host; only its native surface
        // is replaced. The ECS world and hot-loaded project are not recreated.
        config = config
            .with_on_window(move |window, _| {
                if let Err(error) = editor.attach_detached_scene(window) {
                    eprintln!("[editor] Could not attach detached Scene renderer: {error}");
                }
            })
            .with_as_child_window();
    }

    let pending = window().new_window(dom, config);
    spawn(async move {
        // Resolving keeps creation failures visible to Dioxus while ownership
        // of the actual window remains in the shared desktop application.
        let _ = pending.try_resolve().await;
    });
}

/// Render one detached panel and coordinate its native window lifetime.
#[allow(non_snake_case)]
fn DetachedPanelWindow(props: DetachedPanelProps) -> Element {
    let panel = props.panel;
    let editor = Arc::clone(&props.editor);
    let popouts = Arc::clone(&props.popouts);
    let desktop = window();
    let window_id = desktop.window.id();
    let mut stats = use_signal(Stats::default);
    let mut context_menu = use_signal(|| None::<(f64, f64)>);
    let finalized = use_hook(|| Arc::new(AtomicBool::new(false)));

    // A pop-out has its own VirtualDom, so refresh only its local statistics
    // signal rather than coupling it to main-window reconciliation.
    let stats_editor = Arc::clone(&editor);
    use_future(move || {
        let stats_editor = Arc::clone(&stats_editor);
        async move {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                stats.set(stats_editor.current_stats());
            }
        }
    });

    // Scene owns the current renderer surface while detached. Resize events
    // from other windows are ignored by the window-id guard in EditorContext.
    let event_editor = Arc::clone(&editor);
    let event_popouts = Arc::clone(&popouts);
    let event_finalized = Arc::clone(&finalized);
    use_wry_event_handler(move |event, _| match event {
        TaoEvent::WindowEvent {
            event: WindowEvent::Resized(size),
            ..
        } if panel == PanelKind::Scene => {
            event_editor.resize_detached_scene(window_id, size.width, size.height);
        }
        TaoEvent::WindowEvent {
            event: WindowEvent::CloseRequested,
            ..
        } => finalize_popout(
            panel,
            window_id,
            &event_editor,
            &event_popouts,
            &event_finalized,
        ),
        _ => {}
    });

    // Closing the OS window destroys this VirtualDom. Restore the renderer
    // first, then ask the main runtime to put the tab back into its dock tree.
    let close_editor = Arc::clone(&editor);
    let close_finalized = Arc::clone(&finalized);
    use_drop(move || {
        finalize_popout(panel, window_id, &close_editor, &popouts, &close_finalized);
    });

    let root_class = if panel == PanelKind::Scene {
        "dock-detached-root"
    } else {
        "dock-detached-root dock-checker"
    };
    let dock_css = live_dock_css();

    rsx! {
        style { dangerous_inner_html: dock_css }
        div {
            class: root_class,
            tabindex: "0",
            onclick: move |_| context_menu.set(None),
            oncontextmenu: move |event| {
                event.prevent_default();
                let position = event.client_coordinates();
                context_menu.set(Some((position.x, position.y)));
            },
            onkeydown: move |event| {
                if event.data.key().to_string() == "Escape" {
                    context_menu.set(None);
                }
            },
            div {
                class: "dock-detached-panel",
                PanelContent {
                    panel,
                    stats,
                    editor: Arc::clone(&editor),
                }
            }
            if let Some((x, y)) = *context_menu.read() {
                div {
                    class: "dock-context-menu",
                    role: "menu",
                    style: format!("left:{x:.3}px;top:{y:.3}px;"),
                    oncontextmenu: move |event| event.prevent_default(),
                    button {
                        class: "dock-context-menu-item",
                        role: "menuitem",
                        onclick: move |_| context_menu.set(None),
                        "hello1"
                    }
                    button {
                        class: "dock-context-menu-item",
                        role: "menuitem",
                        onclick: move |_| context_menu.set(None),
                        "hello2"
                    }
                }
            }
        }
    }
}

/// Restore all state owned by a pop-out exactly once during window shutdown.
fn finalize_popout(
    panel: PanelKind,
    window_id: dioxus::desktop::tao::window::WindowId,
    editor: &EditorContext,
    popouts: &PopoutManager,
    finalized: &AtomicBool,
) {
    if finalized.swap(true, Ordering::AcqRel) {
        return;
    }
    if panel == PanelKind::Scene {
        if let Err(error) = editor.reattach_main_scene(window_id) {
            eprintln!("[editor] Could not restore the main Scene renderer: {error}");
        }
    }
    popouts.request_redock(panel);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Close notifications cross VirtualDom boundaries once and are drained atomically.
    #[test]
    fn popout_manager_drains_redock_notifications() {
        let manager = PopoutManager::default();
        manager.request_redock(PanelKind::Hierarchy);
        manager.request_redock(PanelKind::Scene);

        assert_eq!(
            manager.drain_redocks(),
            vec![PanelKind::Hierarchy, PanelKind::Scene]
        );
        assert!(manager.drain_redocks().is_empty());
    }
}
